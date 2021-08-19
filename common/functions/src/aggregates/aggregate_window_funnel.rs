// Copyright 2020 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::alloc::Layout;
use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;

use bytes::BytesMut;
use common_datavalues::prelude::*;
use common_exception::ErrorCode;
use common_exception::Result;
use common_io::prelude::*;
use num::traits::AsPrimitive;

use super::AggregateFunctionRef;
use super::StateAddr;
use crate::aggregates::aggregator_common::assert_unary_arguments;
use crate::aggregates::AggregateFunction;
use crate::dispatch_unsigned_numeric_types;

struct AggregateWindowFunnelState<T> {
    pub events_list: Vec<(T, u8)>,
    pub sorted: bool,
}

impl<T> AggregateWindowFunnelState<T>
where T: Ord + AsPrimitive<u64> + BinarySer + BinaryDe + Clone + Send + Sync + 'static
{
    fn new() -> Self {
        Self {
            events_list: Vec::new(),
            sorted: true,
        }
    }
    #[inline(always)]
    fn add(&mut self, timestamp: T, event: u8) {
        if self.sorted && !self.events_list.is_empty() {
            let last = self.events_list.last().unwrap();
            if last.0 == timestamp {
                self.sorted = last.1 <= event;
            } else {
                self.sorted = last.0 <= timestamp;
            }
        }
        self.events_list.push((timestamp, event));
    }

    #[inline(always)]
    fn merge(&mut self, other: &mut Self) {
        if other.events_list.is_empty() {
            return;
        }

        let l1 = self.events_list.len();
        let l2 = other.events_list.len();

        self.sort();
        other.sort();
        let mut merged = Vec::with_capacity(self.events_list.len() + other.events_list.len());
        let cmp = |a: &(T, u8), b: &(T, u8)| {
            let ord = a.0.cmp(&b.0);
            if ord == Ordering::Equal {
                a.1.cmp(&b.1)
            } else {
                ord
            }
        };

        {
            let mut i = 0;
            let mut j = 0;
            let mut k = 0;
            while i < l1 && j < l2 {
                if cmp(&self.events_list[i], &other.events_list[j]) == Ordering::Less {
                    merged.push(self.events_list[i]);
                    k += 1;
                    i += 1;
                } else {
                    merged.push(other.events_list[i]);
                    k += 1;
                    j += 1;
                }
            }
            if i < l1 {
                merged[k..].copy_from_slice(&self.events_list[i..]);
            }
            if j < l2 {
                merged[k..].copy_from_slice(&other.events_list[i..]);
            }
        }
        self.events_list = merged;
    }

    #[inline(always)]
    fn sort(&mut self) {
        let cmp = |a: &(T, u8), b: &(T, u8)| {
            let ord = a.0.cmp(&b.0);
            if ord == Ordering::Equal {
                a.1.cmp(&b.1)
            } else {
                ord
            }
        };
        if !self.sorted {
            self.events_list.sort_by(cmp);
        }
    }

    fn serialize(&self, writer: &mut BytesMut) -> Result<()> {
        self.sorted.serialize_to_buf(writer)?;
        writer.write_uvarint(self.events_list.len() as u64)?;

        for (timestamp, event) in self.events_list.iter() {
            timestamp.serialize_to_buf(writer)?;
            event.serialize_to_buf(writer)?;
        }
        Ok(())
    }

    fn deserialize(&mut self, reader: &mut &[u8]) -> Result<()> {
        self.sorted = bool::deserialize(reader)?;
        let size: u64 = reader.read_uvarint()?;
        self.events_list = Vec::with_capacity(size as usize);

        for _i in 0..size {
            let timestamp = T::deserialize(reader)?;
            let event = u8::deserialize(reader)?;

            self.events_list.push((timestamp, event));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AggregateWindowFunnelFunction<T> {
    display_name: String,
    arguments: Vec<DataField>,
    event_size: usize,
    window: u64,
    t: PhantomData<T>,
}

impl<T> AggregateFunction for AggregateWindowFunnelFunction<T>
where
    T: DFNumericType,
    T::Native: Ord + AsPrimitive<u64> + BinarySer + BinaryDe + Clone + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        "AggregateWindowFunnelFunction"
    }

    fn return_type(&self) -> Result<DataType> {
        Ok(DataType::UInt8)
    }

    fn nullable(&self, _input_schema: &DataSchema) -> Result<bool> {
        Ok(false)
    }

    fn init_state(&self, place: StateAddr) {
        place.write(|| AggregateWindowFunnelState::<T::Native>::new());
    }

    fn state_layout(&self) -> Layout {
        Layout::new::<AggregateWindowFunnelState<T::Native>>()
    }

    fn accumulate(&self, place: StateAddr, arrays: &[Series], _input_rows: usize) -> Result<()> {
        let mut darrays = Vec::with_capacity(self.event_size);
        for i in 0..self.event_size {
            let darray = arrays[i + 1].bool()?.downcast_ref();
            darrays.push(darray);
        }

        let tarray: &DataArray<T> = arrays[0].static_cast();
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();
        for (row, timestmap) in tarray.into_iter().enumerate() {
            if let Some(timestmap) = timestmap {
                for i in 0..self.event_size {
                    if darrays[i].value(row) {
                        state.add(timestmap, (i + 1) as u8);
                    }
                }
            }
        }
        Ok(())
    }

    fn accumulate_keys(
        &self,
        places: &[StateAddr],
        offset: usize,
        arrays: &[Series],
        _input_rows: usize,
    ) -> Result<()> {
        let mut darrays = Vec::with_capacity(self.event_size);
        for i in 0..self.event_size {
            let darray = arrays[i + 1].bool()?.downcast_ref();
            darrays.push(darray);
        }
        let tarray: &DataArray<T> = arrays[0].static_cast();
        for ((row, timestmap), place) in tarray.into_iter().enumerate().zip(places.iter()) {
            if let Some(timestmap) = timestmap {
                let state = (place.next(offset)).get::<AggregateWindowFunnelState<T::Native>>();
                for i in 0..self.event_size {
                    if darrays[i].value(row) {
                        state.add(timestmap, (i + 1) as u8);
                    }
                }
            }
        }
        Ok(())
    }

    fn serialize(&self, place: StateAddr, writer: &mut BytesMut) -> Result<()> {
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();
        state.serialize(writer)
    }

    fn deserialize(&self, place: StateAddr, reader: &mut &[u8]) -> Result<()> {
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();
        state.deserialize(reader)
    }

    fn merge(&self, place: StateAddr, rhs: StateAddr) -> Result<()> {
        let rhs = rhs.get::<AggregateWindowFunnelState<T::Native>>();
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();

        state.merge(rhs);
        Ok(())
    }

    fn merge_result(&self, place: StateAddr) -> Result<DataValue> {
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();
        state.sort();

        todo!();
    }
}

impl<T> fmt::Display for AggregateWindowFunnelFunction<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

impl<T> AggregateWindowFunnelFunction<T>
where
    T: DFNumericType,
    T::Native: Ord + AsPrimitive<u64> + BinarySer + BinaryDe + Clone + Send + Sync + 'static,
{
    pub fn try_create(
        display_name: &str,
        arguments: Vec<DataField>,
    ) -> Result<AggregateFunctionRef> {
        let event_size = arguments.len() - 1;
        Ok(Arc::new(Self {
            display_name: display_name.to_owned(),
            arguments,
            event_size,
            window: 1024,
            t: PhantomData,
        }))
    }

    /// Loop through the entire events_list, update the event timestamp value
    /// The level path must be 1---2---3---...---check_events_size, find the max event level that satisfied the path in the sliding window.
    /// If found, returns the max event level, else return 0.
    /// The Algorithm complexity is O(n).
    fn get_event_level(&mut self, place: StateAddr) -> u8 {
        let state = place.get::<AggregateWindowFunnelState<T::Native>>();
        if state.events_list.is_empty() {
            return 0;
        }
        if self.event_size == 1 {
            return 1;
        }

        state.sort();

        let mut events_timestamp: Vec<Option<u64>> = Vec::with_capacity(self.event_size);
        for _i in 0..self.event_size {
            events_timestamp.push(None);
        }
        let mut first_event = false;

        for (timestamp, event) in state.events_list.iter() {
            let event_idx = event - 1;

            if event_idx == 0 {
                events_timestamp.push(Some(timestamp));
                first_event = true;
            } else if events_timestamp[event_idx - 1].is_some() {
            }
        }

        4
    }
}

macro_rules! creator {
    ($T: ident, $data_type: expr, $display_name: expr, $arguments: expr) => {
        if $T::data_type() == $data_type {
            return AggregateWindowFunnelFunction::<$T>::try_create($display_name, $arguments);
        }
    };
}

pub fn try_create_aggregate_WindowFunnel_function(
    display_name: &str,
    arguments: Vec<DataField>,
) -> Result<AggregateFunctionRef> {
    assert_unary_arguments(display_name, arguments.len())?;

    let data_type = arguments[0].data_type();
    dispatch_unsigned_numeric_types! {creator, data_type.clone(), display_name, arguments}

    Err(ErrorCode::BadDataValueType(format!(
        "AggregateWindowFunnelFunction does not support type '{:?}'",
        data_type
    )))
}
