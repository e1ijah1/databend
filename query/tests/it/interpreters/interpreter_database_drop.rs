// Copyright 2021 Datafuse Labs.
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

use common_base::tokio;
use common_exception::Result;
use common_planners::*;
use databend_query::interpreters::*;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;

use crate::tests::parse_query;

#[tokio::test]
async fn test_drop_database_interpreter() -> Result<()> {
    let ctx = crate::tests::create_query_context()?;

    if let PlanNode::DropDatabase(plan) = parse_query("drop database default", &ctx)? {
        let executor = DropDatabaseInterpreter::try_create(ctx, plan.clone())?;
        assert_eq!(executor.name(), "DropDatabaseInterpreter");
        let stream = executor.execute(None).await?;
        let result = stream.try_collect::<Vec<_>>().await?;
        let expected = vec!["++", "++"];
        common_datablocks::assert_blocks_sorted_eq(expected, result.as_slice());
    } else {
        panic!()
    }

    Ok(())
}