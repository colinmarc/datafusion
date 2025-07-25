// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! This module contains end to end demonstrations of creating
//! user defined aggregate functions

use std::any::Any;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::{size_of, size_of_val};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use arrow::array::{
    record_batch, types::UInt64Type, Array, AsArray, Int32Array, PrimitiveArray,
    StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::{Fields, Schema};
use arrow_schema::FieldRef;
use datafusion::common::test_util::batches_to_string;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::MemTable;
use datafusion::test_util::plan_and_collect;
use datafusion::{
    arrow::{
        array::{ArrayRef, Float64Array, TimestampNanosecondArray},
        datatypes::{DataType, Field, Float64Type, TimeUnit, TimestampNanosecondType},
        record_batch::RecordBatch,
    },
    error::Result,
    logical_expr::{
        AccumulatorFactoryFunction, AggregateUDF, Signature, TypeSignature, Volatility,
    },
    physical_plan::Accumulator,
    prelude::SessionContext,
    scalar::ScalarValue,
};
use datafusion_common::{assert_contains, exec_datafusion_err};
use datafusion_common::{cast::as_primitive_array, exec_err};
use datafusion_expr::expr::WindowFunction;
use datafusion_expr::{
    col, create_udaf, function::AccumulatorArgs, AggregateUDFImpl, Expr,
    GroupsAccumulator, LogicalPlanBuilder, SimpleAggregateUDF, WindowFunctionDefinition,
};
use datafusion_functions_aggregate::average::AvgAccumulator;

/// Test to show the contents of the setup
#[tokio::test]
async fn test_setup() {
    let TestContext { ctx, test_state: _ } = TestContext::new();
    let sql = "SELECT * from t order by time";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +-------+----------------------------+
    | value | time                       |
    +-------+----------------------------+
    | 2.0   | 1970-01-01T00:00:00.000002 |
    | 3.0   | 1970-01-01T00:00:00.000003 |
    | 1.0   | 1970-01-01T00:00:00.000004 |
    | 5.0   | 1970-01-01T00:00:00.000005 |
    | 5.0   | 1970-01-01T00:00:00.000005 |
    +-------+----------------------------+
    "###);
}

/// Basic user defined aggregate
#[tokio::test]
async fn test_udaf() {
    let TestContext { ctx, test_state } = TestContext::new();
    assert!(!test_state.update_batch());
    let sql = "SELECT time_sum(time) from t";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +----------------------------+
    | time_sum(t.time)           |
    +----------------------------+
    | 1970-01-01T00:00:00.000019 |
    +----------------------------+
    "###);

    // normal aggregates call update_batch
    assert!(test_state.update_batch());
    assert!(!test_state.retract_batch());
}

/// User defined aggregate used as a window function
#[tokio::test]
async fn test_udaf_as_window() {
    let TestContext { ctx, test_state } = TestContext::new();
    let sql = "SELECT time_sum(time) OVER() as time_sum from t";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +----------------------------+
    | time_sum                   |
    +----------------------------+
    | 1970-01-01T00:00:00.000019 |
    | 1970-01-01T00:00:00.000019 |
    | 1970-01-01T00:00:00.000019 |
    | 1970-01-01T00:00:00.000019 |
    | 1970-01-01T00:00:00.000019 |
    +----------------------------+
    "###);

    // aggregate over the entire window function call update_batch
    assert!(test_state.update_batch());
    assert!(!test_state.retract_batch());
}

/// User defined aggregate used as a window function with a window frame
#[tokio::test]
async fn test_udaf_as_window_with_frame() {
    let TestContext { ctx, test_state } = TestContext::new();
    let sql = "SELECT time_sum(time) OVER(ORDER BY time ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) as time_sum from t";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +----------------------------+
    | time_sum                   |
    +----------------------------+
    | 1970-01-01T00:00:00.000005 |
    | 1970-01-01T00:00:00.000009 |
    | 1970-01-01T00:00:00.000012 |
    | 1970-01-01T00:00:00.000014 |
    | 1970-01-01T00:00:00.000010 |
    +----------------------------+
    "###);

    // user defined aggregates with window frame should be calling retract batch
    assert!(test_state.update_batch());
    assert!(test_state.retract_batch());
}

/// Ensure that User defined aggregate used as a window function with a window
/// frame, but that does not implement retract_batch, returns an error
#[tokio::test]
async fn test_udaf_as_window_with_frame_without_retract_batch() {
    let test_state = Arc::new(TestState::new().with_error_on_retract_batch());

    let TestContext { ctx, test_state: _ } = TestContext::new_with_test_state(test_state);
    let sql = "SELECT time_sum(time) OVER(ORDER BY time ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) as time_sum from t";
    // Note if this query ever does start working
    let err = execute(&ctx, sql).await.unwrap_err();
    assert_contains!(err.to_string(), "This feature is not implemented: Aggregate can not be used as a sliding accumulator because `retract_batch` is not implemented: time_sum(t.time) ORDER BY [t.time ASC NULLS LAST] ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING");
}

/// Basic query for with a udaf returning a structure
#[tokio::test]
async fn test_udaf_returning_struct() {
    let TestContext { ctx, test_state: _ } = TestContext::new();
    let sql = "SELECT first(value, time) from t";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +------------------------------------------------+
    | first(t.value,t.time)                          |
    +------------------------------------------------+
    | {value: 2.0, time: 1970-01-01T00:00:00.000002} |
    +------------------------------------------------+
    "###);
}

/// Demonstrate extracting the fields from a structure using a subquery
#[tokio::test]
async fn test_udaf_returning_struct_subquery() {
    let TestContext { ctx, test_state: _ } = TestContext::new();
    let sql = "select sq.first['value'], sq.first['time'] from (SELECT first(value, time) as first from t) as sq";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +-----------------+----------------------------+
    | sq.first[value] | sq.first[time]             |
    +-----------------+----------------------------+
    | 2.0             | 1970-01-01T00:00:00.000002 |
    +-----------------+----------------------------+
    "###);
}

#[tokio::test]
async fn test_udaf_shadows_builtin_fn() {
    let TestContext {
        mut ctx,
        test_state,
    } = TestContext::new();
    let sql = "SELECT sum(arrow_cast(time, 'Int64')) from t";

    // compute with builtin `sum` aggregator
    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +---------------------------------------+
    | sum(arrow_cast(t.time,Utf8("Int64"))) |
    +---------------------------------------+
    | 19000                                 |
    +---------------------------------------+
    "###);

    // Register `TimeSum` with name `sum`. This will shadow the builtin one
    TimeSum::register(&mut ctx, test_state.clone(), "sum");
    let sql = "SELECT sum(time) from t";

    let actual = execute(&ctx, sql).await.unwrap();

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +----------------------------+
    | sum(t.time)                |
    +----------------------------+
    | 1970-01-01T00:00:00.000019 |
    +----------------------------+
    "###);
}

async fn execute(ctx: &SessionContext, sql: &str) -> Result<Vec<RecordBatch>> {
    ctx.sql(sql).await?.collect().await
}

/// tests the creation, registration and usage of a UDAF
#[tokio::test]
async fn simple_udaf() -> Result<()> {
    let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);

    let batch1 = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
    )?;
    let batch2 = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![Arc::new(Int32Array::from(vec![4, 5]))],
    )?;

    let ctx = SessionContext::new();

    let provider = MemTable::try_new(Arc::new(schema), vec![vec![batch1], vec![batch2]])?;
    ctx.register_table("t", Arc::new(provider))?;

    // define a udaf, using a DataFusion's accumulator
    let my_avg = create_udaf(
        "my_avg",
        vec![DataType::Float64],
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        Arc::new(|_| Ok(Box::<AvgAccumulator>::default())),
        Arc::new(vec![DataType::UInt64, DataType::Float64]),
    );

    ctx.register_udaf(my_avg);

    let result = ctx.sql("SELECT MY_AVG(a) FROM t").await?.collect().await?;

    insta::assert_snapshot!(batches_to_string(&result), @r###"
    +-------------+
    | my_avg(t.a) |
    +-------------+
    | 3.0         |
    +-------------+
    "###);

    Ok(())
}

#[tokio::test]
async fn deregister_udaf() -> Result<()> {
    let ctx = SessionContext::new();
    let my_avg = create_udaf(
        "my_avg",
        vec![DataType::Float64],
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        Arc::new(|_| Ok(Box::<AvgAccumulator>::default())),
        Arc::new(vec![DataType::UInt64, DataType::Float64]),
    );

    ctx.register_udaf(my_avg);

    assert!(ctx.state().aggregate_functions().contains_key("my_avg"));

    ctx.deregister_udaf("my_avg");

    assert!(!ctx.state().aggregate_functions().contains_key("my_avg"));

    Ok(())
}

#[tokio::test]
async fn case_sensitive_identifiers_user_defined_aggregates() -> Result<()> {
    let ctx = SessionContext::new();
    let arr = Int32Array::from(vec![1]);
    let batch = RecordBatch::try_from_iter(vec![("i", Arc::new(arr) as _)])?;
    ctx.register_batch("t", batch).unwrap();

    // Note capitalization
    let my_avg = create_udaf(
        "MY_AVG",
        vec![DataType::Float64],
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        Arc::new(|_| Ok(Box::<AvgAccumulator>::default())),
        Arc::new(vec![DataType::UInt64, DataType::Float64]),
    );

    ctx.register_udaf(my_avg);

    // doesn't work as it was registered as non lowercase
    let err = ctx.sql("SELECT MY_AVG(i) FROM t").await.unwrap_err();
    assert!(err
        .to_string()
        .contains("Error during planning: Invalid function \'my_avg\'"));

    // Can call it if you put quotes
    let result = ctx
        .sql("SELECT \"MY_AVG\"(i) FROM t")
        .await?
        .collect()
        .await?;

    insta::assert_snapshot!(batches_to_string(&result), @r###"
    +-------------+
    | MY_AVG(t.i) |
    +-------------+
    | 1.0         |
    +-------------+
    "###);

    Ok(())
}

#[tokio::test]
async fn test_user_defined_functions_with_alias() -> Result<()> {
    let ctx = SessionContext::new();
    let arr = Int32Array::from(vec![1]);
    let batch = RecordBatch::try_from_iter(vec![("i", Arc::new(arr) as _)])?;
    ctx.register_batch("t", batch).unwrap();

    let my_avg = create_udaf(
        "dummy",
        vec![DataType::Float64],
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        Arc::new(|_| Ok(Box::<AvgAccumulator>::default())),
        Arc::new(vec![DataType::UInt64, DataType::Float64]),
    )
    .with_aliases(vec!["dummy_alias"]);

    ctx.register_udaf(my_avg);

    let result = plan_and_collect(&ctx, "SELECT dummy(i) FROM t").await?;

    insta::assert_snapshot!(batches_to_string(&result), @r###"
    +------------+
    | dummy(t.i) |
    +------------+
    | 1.0        |
    +------------+
    "###);

    let alias_result = plan_and_collect(&ctx, "SELECT dummy_alias(i) FROM t").await?;

    insta::assert_snapshot!(batches_to_string(&alias_result), @r###"
    +------------+
    | dummy(t.i) |
    +------------+
    | 1.0        |
    +------------+
    "###);

    Ok(())
}

#[tokio::test]
async fn test_groups_accumulator() -> Result<()> {
    let ctx = SessionContext::new();
    let arr = Int32Array::from(vec![1]);
    let batch = RecordBatch::try_from_iter(vec![("a", Arc::new(arr) as _)])?;
    ctx.register_batch("t", batch).unwrap();

    let udaf = AggregateUDF::from(TestGroupsAccumulator {
        signature: Signature::exact(vec![DataType::Float64], Volatility::Immutable),
        result: 1,
    });
    ctx.register_udaf(udaf.clone());

    let sql_df = ctx.sql("SELECT geo_mean(a) FROM t group by a").await?;
    sql_df.show().await?;

    Ok(())
}

#[tokio::test]
async fn test_parameterized_aggregate_udf() -> Result<()> {
    let batch = RecordBatch::try_from_iter([(
        "text",
        Arc::new(StringArray::from(vec!["foo"])) as ArrayRef,
    )])?;

    let ctx = SessionContext::new();
    ctx.register_batch("t", batch)?;
    let t = ctx.table("t").await?;
    let signature = Signature::exact(vec![DataType::Utf8], Volatility::Immutable);
    let udf1 = AggregateUDF::from(TestGroupsAccumulator {
        signature: signature.clone(),
        result: 1,
    });
    let udf2 = AggregateUDF::from(TestGroupsAccumulator {
        signature: signature.clone(),
        result: 2,
    });

    let plan = LogicalPlanBuilder::from(t.into_optimized_plan()?)
        .aggregate(
            [col("text")],
            [
                udf1.call(vec![col("text")]).alias("a"),
                udf2.call(vec![col("text")]).alias("b"),
            ],
        )?
        .build()?;

    assert_eq!(
        format!("{plan}"),
        "Aggregate: groupBy=[[t.text]], aggr=[[geo_mean(t.text) AS a, geo_mean(t.text) AS b]]\n  TableScan: t projection=[text]"
    );

    let actual = DataFrame::new(ctx.state(), plan).collect().await?;

    insta::assert_snapshot!(batches_to_string(&actual), @r###"
    +------+---+---+
    | text | a | b |
    +------+---+---+
    | foo  | 1 | 2 |
    +------+---+---+
    "###);

    ctx.deregister_table("t")?;
    Ok(())
}

/// Returns an context with a table "t" and the "first" and "time_sum"
/// aggregate functions registered.
///
/// "t" contains this data:
///
/// ```text
/// value | time
///  3.0  | 1970-01-01T00:00:00.000003
///  2.0  | 1970-01-01T00:00:00.000002
///  1.0  | 1970-01-01T00:00:00.000004
///  5.0  | 1970-01-01T00:00:00.000005
///  5.0  | 1970-01-01T00:00:00.000005
/// ```
struct TestContext {
    ctx: SessionContext,
    test_state: Arc<TestState>,
}

impl TestContext {
    fn new() -> Self {
        let test_state = Arc::new(TestState::new());
        Self::new_with_test_state(test_state)
    }

    fn new_with_test_state(test_state: Arc<TestState>) -> Self {
        let value = Float64Array::from(vec![3.0, 2.0, 1.0, 5.0, 5.0]);
        let time = TimestampNanosecondArray::from(vec![3000, 2000, 4000, 5000, 5000]);

        let batch = RecordBatch::try_from_iter(vec![
            ("value", Arc::new(value) as _),
            ("time", Arc::new(time) as _),
        ])
        .unwrap();

        let mut ctx = SessionContext::new();

        ctx.register_batch("t", batch).unwrap();

        // Tell DataFusion about the "first" function
        FirstSelector::register(&mut ctx);
        // Tell DataFusion about the "time_sum" function
        TimeSum::register(&mut ctx, Arc::clone(&test_state), "time_sum");

        Self { ctx, test_state }
    }
}

#[derive(Debug, Default)]
struct TestState {
    /// was update_batch called?
    update_batch: AtomicBool,
    /// was retract_batch called?
    retract_batch: AtomicBool,
    /// should the udaf throw an error if retract batch is called? Can
    /// only be configured at construction time.
    error_on_retract_batch: bool,
}

impl TestState {
    fn new() -> Self {
        Default::default()
    }

    /// Has `update_batch` been called?
    fn update_batch(&self) -> bool {
        self.update_batch.load(Ordering::SeqCst)
    }

    /// Set the `update_batch` flag
    fn set_update_batch(&self) {
        self.update_batch.store(true, Ordering::SeqCst)
    }

    /// Has `retract_batch` been called?
    fn retract_batch(&self) -> bool {
        self.retract_batch.load(Ordering::SeqCst)
    }

    /// set the `retract_batch` flag
    fn set_retract_batch(&self) {
        self.retract_batch.store(true, Ordering::SeqCst)
    }

    /// Is this state configured to return an error on retract batch?
    fn error_on_retract_batch(&self) -> bool {
        self.error_on_retract_batch
    }

    /// Configure the test to return error on retract batch
    fn with_error_on_retract_batch(mut self) -> Self {
        self.error_on_retract_batch = true;
        self
    }
}

/// Models a user defined aggregate function that computes the a sum
/// of timestamps (not a quantity that has much real world meaning)
#[derive(Debug)]
struct TimeSum {
    sum: i64,
    test_state: Arc<TestState>,
}

impl TimeSum {
    fn new(test_state: Arc<TestState>) -> Self {
        Self { sum: 0, test_state }
    }

    fn register(ctx: &mut SessionContext, test_state: Arc<TestState>, name: &str) {
        let timestamp_type = DataType::Timestamp(TimeUnit::Nanosecond, None);
        let input_type = vec![timestamp_type.clone()];

        // Returns the same type as its input
        let return_type = timestamp_type.clone();

        let state_fields = vec![Field::new("sum", timestamp_type, true).into()];

        let volatility = Volatility::Immutable;

        let captured_state = Arc::clone(&test_state);
        let accumulator: AccumulatorFactoryFunction =
            Arc::new(move |_| Ok(Box::new(Self::new(Arc::clone(&captured_state)))));

        let time_sum = AggregateUDF::from(SimpleAggregateUDF::new(
            name,
            input_type,
            return_type,
            volatility,
            accumulator,
            state_fields,
        ));

        // register the selector as "time_sum"
        ctx.register_udaf(time_sum)
    }
}

impl Accumulator for TimeSum {
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![self.evaluate()?])
    }

    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        self.test_state.set_update_batch();
        assert_eq!(values.len(), 1);
        let arr = &values[0];
        let arr = arr.as_primitive::<TimestampNanosecondType>();

        for v in arr.values().iter() {
            self.sum += v;
        }
        Ok(())
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        // merge and update is the same for time sum
        self.update_batch(states)
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        Ok(ScalarValue::TimestampNanosecond(Some(self.sum), None))
    }

    fn size(&self) -> usize {
        // accurate size estimates are not important for this example
        42
    }

    fn retract_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        if self.test_state.error_on_retract_batch() {
            return exec_err!("Error in Retract Batch");
        }

        self.test_state.set_retract_batch();
        assert_eq!(values.len(), 1);
        let arr = &values[0];
        let arr = arr.as_primitive::<TimestampNanosecondType>();

        for v in arr.values().iter() {
            self.sum -= v;
        }
        Ok(())
    }

    fn supports_retract_batch(&self) -> bool {
        !self.test_state.error_on_retract_batch()
    }
}

/// Models a specialized timeseries aggregate function
/// called a "selector" in InfluxQL and Flux.
///
/// It returns the value and corresponding timestamp of the
/// input with the earliest timestamp as a structure.
#[derive(Debug, Clone)]
struct FirstSelector {
    value: f64,
    time: i64,
}

impl FirstSelector {
    /// Create a new empty selector
    fn new() -> Self {
        Self {
            value: 0.0,
            time: i64::MAX,
        }
    }

    fn register(ctx: &mut SessionContext) {
        let return_type = Self::output_datatype();
        let state_type = Self::state_datatypes();
        let state_fields = state_type
            .into_iter()
            .enumerate()
            .map(|(i, t)| Field::new(format!("{i}"), t, true).into())
            .collect::<Vec<_>>();

        // Possible input signatures
        let signatures = vec![TypeSignature::Exact(Self::input_datatypes())];

        let accumulator: AccumulatorFactoryFunction =
            Arc::new(|_| Ok(Box::new(Self::new())));

        let volatility = Volatility::Immutable;

        let name = "first";

        let first = AggregateUDF::from(SimpleAggregateUDF::new_with_signature(
            name,
            Signature::one_of(signatures, volatility),
            return_type,
            accumulator,
            state_fields,
        ));

        // register the selector as "first"
        ctx.register_udaf(first)
    }

    /// Return the schema fields
    fn fields() -> Fields {
        vec![
            Field::new("value", DataType::Float64, true),
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            ),
        ]
        .into()
    }

    fn output_datatype() -> DataType {
        DataType::Struct(Self::fields())
    }

    fn input_datatypes() -> Vec<DataType> {
        vec![
            DataType::Float64,
            DataType::Timestamp(TimeUnit::Nanosecond, None),
        ]
    }

    // Internally, keep the data types as this type
    fn state_datatypes() -> Vec<DataType> {
        vec![Self::output_datatype()]
    }

    /// Convert to a set of ScalarValues
    fn to_state(&self) -> Result<ScalarValue> {
        let f64arr = Arc::new(Float64Array::from(vec![self.value])) as ArrayRef;
        let timearr =
            Arc::new(TimestampNanosecondArray::from(vec![self.time])) as ArrayRef;

        let struct_arr =
            StructArray::try_new(Self::fields(), vec![f64arr, timearr], None)?;
        Ok(ScalarValue::Struct(Arc::new(struct_arr)))
    }
}

impl Accumulator for FirstSelector {
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        self.evaluate().map(|s| vec![s])
    }

    /// produce the output structure
    fn evaluate(&mut self) -> Result<ScalarValue> {
        self.to_state()
    }

    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        // cast arguments to the appropriate type (DataFusion will type
        // check these based on the declared allowed input types)
        let v = as_primitive_array::<Float64Type>(&values[0])?;
        let t = as_primitive_array::<TimestampNanosecondType>(&values[1])?;

        // Update the actual values
        for (value, time) in v.iter().zip(t.iter()) {
            if let (Some(time), Some(value)) = (time, value) {
                if time < self.time {
                    self.value = value;
                    self.time = time;
                }
            }
        }

        Ok(())
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        // same logic is needed as in update_batch
        self.update_batch(states)
    }

    fn size(&self) -> usize {
        size_of_val(self)
    }
}

#[derive(Debug, Clone)]
struct TestGroupsAccumulator {
    signature: Signature,
    result: u64,
}

impl AggregateUDFImpl for TestGroupsAccumulator {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "geo_mean"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::UInt64)
    }

    fn accumulator(&self, _acc_args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        // should use groups accumulator
        panic!("accumulator shouldn't invoke");
    }

    fn groups_accumulator_supported(&self, _args: AccumulatorArgs) -> bool {
        true
    }

    fn create_groups_accumulator(
        &self,
        _args: AccumulatorArgs,
    ) -> Result<Box<dyn GroupsAccumulator>> {
        Ok(Box::new(self.clone()))
    }

    fn equals(&self, other: &dyn AggregateUDFImpl) -> bool {
        if let Some(other) = other.as_any().downcast_ref::<TestGroupsAccumulator>() {
            self.result == other.result && self.signature == other.signature
        } else {
            false
        }
    }

    fn hash_value(&self) -> u64 {
        let hasher = &mut DefaultHasher::new();
        self.signature.hash(hasher);
        self.result.hash(hasher);
        hasher.finish()
    }
}

impl Accumulator for TestGroupsAccumulator {
    fn update_batch(&mut self, _values: &[ArrayRef]) -> Result<()> {
        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        Ok(ScalarValue::from(self.result))
    }

    fn size(&self) -> usize {
        size_of::<u64>()
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![ScalarValue::from(self.result)])
    }

    fn merge_batch(&mut self, _states: &[ArrayRef]) -> Result<()> {
        Ok(())
    }
}

impl GroupsAccumulator for TestGroupsAccumulator {
    fn update_batch(
        &mut self,
        _values: &[ArrayRef],
        _group_indices: &[usize],
        _opt_filter: Option<&arrow::array::BooleanArray>,
        _total_num_groups: usize,
    ) -> Result<()> {
        Ok(())
    }

    fn evaluate(&mut self, _emit_to: datafusion_expr::EmitTo) -> Result<ArrayRef> {
        Ok(Arc::new(PrimitiveArray::<UInt64Type>::new(
            vec![self.result].into(),
            None,
        )) as ArrayRef)
    }

    fn state(&mut self, _emit_to: datafusion_expr::EmitTo) -> Result<Vec<ArrayRef>> {
        Ok(vec![Arc::new(PrimitiveArray::<UInt64Type>::new(
            vec![self.result].into(),
            None,
        )) as ArrayRef])
    }

    fn merge_batch(
        &mut self,
        _values: &[ArrayRef],
        _group_indices: &[usize],
        _opt_filter: Option<&arrow::array::BooleanArray>,
        _total_num_groups: usize,
    ) -> Result<()> {
        Ok(())
    }

    fn size(&self) -> usize {
        size_of::<u64>()
    }
}

#[derive(Debug)]
struct MetadataBasedAggregateUdf {
    name: String,
    signature: Signature,
    metadata: HashMap<String, String>,
}

impl MetadataBasedAggregateUdf {
    fn new(metadata: HashMap<String, String>) -> Self {
        // The name we return must be unique. Otherwise we will not call distinct
        // instances of this UDF. This is a small hack for the unit tests to get unique
        // names, but you could do something more elegant with the metadata.
        let name = format!("metadata_based_udf_{}", metadata.len());
        Self {
            name,
            signature: Signature::exact(vec![DataType::UInt64], Volatility::Immutable),
            metadata,
        }
    }
}

impl AggregateUDFImpl for MetadataBasedAggregateUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        unimplemented!("this should never be called since return_field is implemented");
    }

    fn return_field(&self, _arg_fields: &[FieldRef]) -> Result<FieldRef> {
        Ok(Field::new(self.name(), DataType::UInt64, true)
            .with_metadata(self.metadata.clone())
            .into())
    }

    fn accumulator(&self, acc_args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        let input_expr = acc_args
            .exprs
            .first()
            .ok_or(exec_datafusion_err!("Expected one argument"))?;
        let input_field = input_expr.return_field(acc_args.schema)?;

        let double_output = input_field
            .metadata()
            .get("modify_values")
            .map(|v| v == "double_output")
            .unwrap_or(false);

        Ok(Box::new(MetadataBasedAccumulator {
            double_output,
            curr_sum: 0,
        }))
    }

    fn equals(&self, other: &dyn AggregateUDFImpl) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        let Self {
            name,
            signature,
            metadata,
        } = self;
        name == &other.name
            && signature == &other.signature
            && metadata == &other.metadata
    }

    fn hash_value(&self) -> u64 {
        let Self {
            name,
            signature,
            metadata: _, // unhashable
        } = self;
        let mut hasher = DefaultHasher::new();
        std::any::type_name::<Self>().hash(&mut hasher);
        name.hash(&mut hasher);
        signature.hash(&mut hasher);
        hasher.finish()
    }
}

#[derive(Debug)]
struct MetadataBasedAccumulator {
    double_output: bool,
    curr_sum: u64,
}

impl Accumulator for MetadataBasedAccumulator {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let arr = values[0]
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or(exec_datafusion_err!("Expected UInt64Array"))?;

        self.curr_sum = arr.iter().fold(self.curr_sum, |a, b| a + b.unwrap_or(0));

        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let v = match self.double_output {
            true => self.curr_sum * 2,
            false => self.curr_sum,
        };

        Ok(ScalarValue::from(v))
    }

    fn size(&self) -> usize {
        9
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![ScalarValue::from(self.curr_sum)])
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        self.update_batch(states)
    }
}

#[tokio::test]
async fn test_metadata_based_aggregate() -> Result<()> {
    let data_array = Arc::new(UInt64Array::from(vec![0, 5, 10, 15, 20])) as ArrayRef;
    let schema = Arc::new(Schema::new(vec![
        Field::new("no_metadata", DataType::UInt64, true),
        Field::new("with_metadata", DataType::UInt64, true).with_metadata(
            [("modify_values".to_string(), "double_output".to_string())]
                .into_iter()
                .collect(),
        ),
    ]));

    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::clone(&data_array), Arc::clone(&data_array)],
    )?;

    let ctx = SessionContext::new();
    ctx.register_batch("t", batch)?;
    let df = ctx.table("t").await?;

    let no_output_meta_udf =
        AggregateUDF::from(MetadataBasedAggregateUdf::new(HashMap::new()));
    let with_output_meta_udf = AggregateUDF::from(MetadataBasedAggregateUdf::new(
        [("output_metatype".to_string(), "custom_value".to_string())]
            .into_iter()
            .collect(),
    ));

    let df = df.aggregate(
        vec![],
        vec![
            no_output_meta_udf
                .call(vec![col("no_metadata")])
                .alias("meta_no_in_no_out"),
            no_output_meta_udf
                .call(vec![col("with_metadata")])
                .alias("meta_with_in_no_out"),
            with_output_meta_udf
                .call(vec![col("no_metadata")])
                .alias("meta_no_in_with_out"),
            with_output_meta_udf
                .call(vec![col("with_metadata")])
                .alias("meta_with_in_with_out"),
        ],
    )?;

    let actual = df.collect().await?;

    // To test for output metadata handling, we set the expected values on the result
    // To test for input metadata handling, we check the numbers returned
    let mut output_meta = HashMap::new();
    let _ = output_meta.insert("output_metatype".to_string(), "custom_value".to_string());
    let expected_schema = Schema::new(vec![
        Field::new("meta_no_in_no_out", DataType::UInt64, true),
        Field::new("meta_with_in_no_out", DataType::UInt64, true),
        Field::new("meta_no_in_with_out", DataType::UInt64, true)
            .with_metadata(output_meta.clone()),
        Field::new("meta_with_in_with_out", DataType::UInt64, true)
            .with_metadata(output_meta.clone()),
    ]);

    let expected = record_batch!(
        ("meta_no_in_no_out", UInt64, [50]),
        ("meta_with_in_no_out", UInt64, [100]),
        ("meta_no_in_with_out", UInt64, [50]),
        ("meta_with_in_with_out", UInt64, [100])
    )?
    .with_schema(Arc::new(expected_schema))?;

    assert_eq!(expected, actual[0]);

    Ok(())
}

#[tokio::test]
async fn test_metadata_based_aggregate_as_window() -> Result<()> {
    let data_array = Arc::new(UInt64Array::from(vec![0, 5, 10, 15, 20])) as ArrayRef;
    let schema = Arc::new(Schema::new(vec![
        Field::new("no_metadata", DataType::UInt64, true),
        Field::new("with_metadata", DataType::UInt64, true).with_metadata(
            [("modify_values".to_string(), "double_output".to_string())]
                .into_iter()
                .collect(),
        ),
    ]));

    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::clone(&data_array), Arc::clone(&data_array)],
    )?;

    let ctx = SessionContext::new();
    ctx.register_batch("t", batch)?;
    let df = ctx.table("t").await?;

    let no_output_meta_udf = Arc::new(AggregateUDF::from(
        MetadataBasedAggregateUdf::new(HashMap::new()),
    ));
    let with_output_meta_udf =
        Arc::new(AggregateUDF::from(MetadataBasedAggregateUdf::new(
            [("output_metatype".to_string(), "custom_value".to_string())]
                .into_iter()
                .collect(),
        )));

    let df = df.select(vec![
        Expr::from(WindowFunction::new(
            WindowFunctionDefinition::AggregateUDF(Arc::clone(&no_output_meta_udf)),
            vec![col("no_metadata")],
        ))
        .alias("meta_no_in_no_out"),
        Expr::from(WindowFunction::new(
            WindowFunctionDefinition::AggregateUDF(no_output_meta_udf),
            vec![col("with_metadata")],
        ))
        .alias("meta_with_in_no_out"),
        Expr::from(WindowFunction::new(
            WindowFunctionDefinition::AggregateUDF(Arc::clone(&with_output_meta_udf)),
            vec![col("no_metadata")],
        ))
        .alias("meta_no_in_with_out"),
        Expr::from(WindowFunction::new(
            WindowFunctionDefinition::AggregateUDF(with_output_meta_udf),
            vec![col("with_metadata")],
        ))
        .alias("meta_with_in_with_out"),
    ])?;

    let actual = df.collect().await?;

    // To test for output metadata handling, we set the expected values on the result
    // To test for input metadata handling, we check the numbers returned
    let mut output_meta = HashMap::new();
    let _ = output_meta.insert("output_metatype".to_string(), "custom_value".to_string());
    let expected_schema = Schema::new(vec![
        Field::new("meta_no_in_no_out", DataType::UInt64, true),
        Field::new("meta_with_in_no_out", DataType::UInt64, true),
        Field::new("meta_no_in_with_out", DataType::UInt64, true)
            .with_metadata(output_meta.clone()),
        Field::new("meta_with_in_with_out", DataType::UInt64, true)
            .with_metadata(output_meta.clone()),
    ]);

    let expected = record_batch!(
        ("meta_no_in_no_out", UInt64, [50, 50, 50, 50, 50]),
        ("meta_with_in_no_out", UInt64, [100, 100, 100, 100, 100]),
        ("meta_no_in_with_out", UInt64, [50, 50, 50, 50, 50]),
        ("meta_with_in_with_out", UInt64, [100, 100, 100, 100, 100])
    )?
    .with_schema(Arc::new(expected_schema))?;

    assert_eq!(expected, actual[0]);

    Ok(())
}
