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

use arrow::array::{
    ArrayRef, FixedSizeListArray, Int32Builder, MapArray, MapBuilder, StringBuilder,
};
use arrow::datatypes::{
    DataType, Field, FieldRef, Fields, Int32Type, IntervalDayTimeType,
    IntervalMonthDayNanoType, IntervalUnit, Schema, SchemaRef, TimeUnit, UnionFields,
    UnionMode, DECIMAL256_MAX_PRECISION,
};
use arrow::util::pretty::pretty_format_batches;
use datafusion::datasource::file_format::json::{JsonFormat, JsonFormatFactory};
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::execution::options::ArrowReadOptions;
use datafusion::optimizer::eliminate_nested_union::EliminateNestedUnion;
use datafusion::optimizer::Optimizer;
use datafusion_common::parsers::CompressionTypeVariant;
use prost::Message;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::mem::size_of_val;
use std::sync::Arc;
use std::vec;

use datafusion::catalog::{TableProvider, TableProviderFactory};
use datafusion::datasource::file_format::arrow::ArrowFormatFactory;
use datafusion::datasource::file_format::csv::CsvFormatFactory;
use datafusion::datasource::file_format::parquet::ParquetFormatFactory;
use datafusion::datasource::file_format::{format_as_file_type, DefaultFileType};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::execution::FunctionRegistry;
use datafusion::functions_aggregate::count::count_udaf;
use datafusion::functions_aggregate::expr_fn::{
    approx_median, approx_percentile_cont, approx_percentile_cont_with_weight, count,
    count_distinct, covar_pop, covar_samp, first_value, grouping, max, median, min,
    stddev, stddev_pop, sum, var_pop, var_sample,
};
use datafusion::functions_aggregate::min_max::max_udaf;
use datafusion::functions_nested::map::map;
use datafusion::functions_window;
use datafusion::functions_window::expr_fn::{
    cume_dist, dense_rank, lag, lead, ntile, percent_rank, rank, row_number,
};
use datafusion::functions_window::rank::rank_udwf;
use datafusion::prelude::*;
use datafusion::test_util::{TestTableFactory, TestTableProvider};
use datafusion_common::config::TableOptions;
use datafusion_common::scalar::ScalarStructBuilder;
use datafusion_common::{
    internal_datafusion_err, internal_err, not_impl_err, plan_err, DFSchema, DFSchemaRef,
    DataFusionError, Result, ScalarValue, TableReference,
};
use datafusion_expr::dml::CopyTo;
use datafusion_expr::expr::{
    self, Between, BinaryExpr, Case, Cast, GroupingSet, InList, Like, ScalarFunction,
    Unnest, WildcardOptions,
};
use datafusion_expr::logical_plan::{Extension, UserDefinedLogicalNodeCore};
use datafusion_expr::{
    Accumulator, AggregateUDF, ColumnarValue, ExprFunctionExt, ExprSchemable, Literal,
    LogicalPlan, Operator, PartitionEvaluator, ScalarUDF, Signature, TryCast, Volatility,
    WindowFrame, WindowFrameBound, WindowFrameUnits, WindowFunctionDefinition, WindowUDF,
    WindowUDFImpl,
};
use datafusion_functions_aggregate::average::avg_udaf;
use datafusion_functions_aggregate::expr_fn::{
    approx_distinct, array_agg, avg, bit_and, bit_or, bit_xor, bool_and, bool_or, corr,
    nth_value,
};
use datafusion_functions_aggregate::string_agg::string_agg;
use datafusion_functions_window_common::field::WindowUDFFieldArgs;
use datafusion_functions_window_common::partition::PartitionEvaluatorArgs;
use datafusion_proto::bytes::{
    logical_plan_from_bytes, logical_plan_from_bytes_with_extension_codec,
    logical_plan_to_bytes, logical_plan_to_bytes_with_extension_codec,
};
use datafusion_proto::logical_plan::file_formats::{
    ArrowLogicalExtensionCodec, CsvLogicalExtensionCodec, JsonLogicalExtensionCodec,
    ParquetLogicalExtensionCodec,
};
use datafusion_proto::logical_plan::to_proto::serialize_expr;
use datafusion_proto::logical_plan::{
    from_proto, DefaultLogicalExtensionCodec, LogicalExtensionCodec,
};
use datafusion_proto::protobuf;

use crate::cases::{MyAggregateUDF, MyAggregateUdfNode, MyRegexUdf, MyRegexUdfNode};

#[cfg(feature = "json")]
fn roundtrip_json_test(proto: &protobuf::LogicalExprNode) {
    let string = serde_json::to_string(proto).unwrap();
    let back: protobuf::LogicalExprNode = serde_json::from_str(&string).unwrap();
    assert_eq!(proto, &back);
}

#[cfg(not(feature = "json"))]
fn roundtrip_json_test(_proto: &protobuf::LogicalExprNode) {}

fn roundtrip_expr_test(initial_struct: Expr, ctx: SessionContext) {
    let extension_codec = DefaultLogicalExtensionCodec {};
    roundtrip_expr_test_with_codec(initial_struct, ctx, &extension_codec);
}

// Given a DataFusion logical Expr, convert it to protobuf and back, using debug formatting to test
// equality.
fn roundtrip_expr_test_with_codec(
    initial_struct: Expr,
    ctx: SessionContext,
    codec: &dyn LogicalExtensionCodec,
) {
    let proto: protobuf::LogicalExprNode = serialize_expr(&initial_struct, codec)
        .unwrap_or_else(|e| panic!("Error serializing expression: {e:?}"));
    let round_trip: Expr = from_proto::parse_expr(&proto, &ctx, codec).unwrap();

    assert_eq!(format!("{:?}", &initial_struct), format!("{round_trip:?}"));

    roundtrip_json_test(&proto);
}

fn new_arc_field(name: &str, dt: DataType, nullable: bool) -> Arc<Field> {
    Arc::new(Field::new(name, dt, nullable))
}

#[tokio::test]
async fn roundtrip_logical_plan() -> Result<()> {
    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    let scan = ctx.table("t1").await?.into_optimized_plan()?;
    let topk_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(TopKPlanNode::new(3, scan, col("revenue"))),
    });
    let extension_codec = TopKExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&topk_plan, &extension_codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &extension_codec)?;
    assert_eq!(format!("{topk_plan:?}"), format!("{logical_round_trip:?}"));
    Ok(())
}

#[derive(Clone, PartialEq, Eq, ::prost::Message)]
pub struct TestTableProto {
    /// URL of the table root
    #[prost(string, tag = "1")]
    pub url: String,
    /// Qualified table name
    #[prost(string, tag = "2")]
    pub table_name: String,
}

#[derive(Debug)]
pub struct TestTableProviderCodec {}

impl LogicalExtensionCodec for TestTableProviderCodec {
    fn try_decode(
        &self,
        _buf: &[u8],
        _inputs: &[LogicalPlan],
        _ctx: &SessionContext,
    ) -> Result<Extension> {
        not_impl_err!("No extension codec provided")
    }

    fn try_encode(&self, _node: &Extension, _buf: &mut Vec<u8>) -> Result<()> {
        not_impl_err!("No extension codec provided")
    }

    fn try_decode_table_provider(
        &self,
        buf: &[u8],
        table_ref: &TableReference,
        schema: SchemaRef,
        _ctx: &SessionContext,
    ) -> Result<Arc<dyn TableProvider>> {
        let msg = TestTableProto::decode(buf).map_err(|_| {
            DataFusionError::Internal("Error decoding test table".to_string())
        })?;
        assert_eq!(msg.table_name, table_ref.to_string());
        let provider = TestTableProvider {
            url: msg.url,
            schema,
        };
        Ok(Arc::new(provider))
    }

    fn try_encode_table_provider(
        &self,
        table_ref: &TableReference,
        node: Arc<dyn TableProvider>,
        buf: &mut Vec<u8>,
    ) -> Result<()> {
        let table = node
            .as_ref()
            .as_any()
            .downcast_ref::<TestTableProvider>()
            .expect("Can't encode non-test tables");
        let msg = TestTableProto {
            url: table.url.clone(),
            table_name: table_ref.to_string(),
        };
        msg.encode(buf).map_err(|_| {
            DataFusionError::Internal("Error encoding test table".to_string())
        })
    }
}

#[tokio::test]
async fn roundtrip_custom_tables() -> Result<()> {
    let mut table_factories: HashMap<String, Arc<dyn TableProviderFactory>> =
        HashMap::new();
    table_factories.insert("TESTTABLE".to_string(), Arc::new(TestTableFactory {}));
    let mut state = SessionStateBuilder::new().with_default_features().build();
    // replace factories
    *state.table_factories_mut() = table_factories;
    let ctx = SessionContext::new_with_state(state);

    let sql = "CREATE EXTERNAL TABLE t STORED AS testtable LOCATION 's3://bucket/schema/table';";
    ctx.sql(sql).await.unwrap();

    let codec = TestTableProviderCodec {};
    let scan = ctx.table("t").await?.into_optimized_plan()?;
    let bytes = logical_plan_to_bytes_with_extension_codec(&scan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{scan:?}"), format!("{logical_round_trip:?}"));
    Ok(())
}

#[tokio::test]
async fn roundtrip_custom_memory_tables() -> Result<()> {
    let ctx = SessionContext::new();
    // Make sure during round-trip, constraint information is preserved
    let query = "CREATE TABLE sales_global_with_pk (zip_code INT,
          country VARCHAR(3),
          sn INT,
          ts TIMESTAMP,
          currency VARCHAR(3),
          amount FLOAT,
          primary key(sn)
        ) as VALUES
          (0, 'GRC', 0, '2022-01-01 06:00:00'::timestamp, 'EUR', 30.0),
          (1, 'FRA', 1, '2022-01-01 08:00:00'::timestamp, 'EUR', 50.0),
          (1, 'TUR', 2, '2022-01-01 11:30:00'::timestamp, 'TRY', 75.0),
          (1, 'FRA', 3, '2022-01-02 12:00:00'::timestamp, 'EUR', 200.0),
          (1, 'TUR', 4, '2022-01-03 10:00:00'::timestamp, 'TRY', 100.0)";

    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan:?}"), format!("{logical_round_trip:?}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_custom_listing_tables() -> Result<()> {
    let ctx = SessionContext::new();

    let query = "CREATE EXTERNAL TABLE multiple_ordered_table_with_pk (
              a0 INTEGER,
              a INTEGER DEFAULT 1*2 + 3,
              b INTEGER DEFAULT NULL,
              c INTEGER,
              d INTEGER,
              primary key(c)
            )
            STORED AS CSV
            WITH ORDER (a ASC, b ASC)
            WITH ORDER (c ASC)
            LOCATION '../core/tests/data/window_2.csv'
            OPTIONS ('format.has_header' 'true')";

    let plan = ctx.state().create_logical_plan(query).await?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    // Use exact matching to verify everything. Make sure during round-trip,
    // information like constraints, column defaults, and other aspects of the plan are preserved.
    assert_eq!(plan, logical_round_trip);

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_aggregation_with_pk() -> Result<()> {
    let ctx = SessionContext::new();

    ctx.sql(
        "CREATE EXTERNAL TABLE multiple_ordered_table_with_pk (
              a0 INTEGER,
              a INTEGER,
              b INTEGER,
              c INTEGER,
              d INTEGER,
              primary key(c)
            )
            STORED AS CSV
            WITH ORDER (a ASC, b ASC)
            WITH ORDER (c ASC)
            LOCATION '../core/tests/data/window_2.csv'
            OPTIONS ('format.has_header' 'true')",
    )
    .await?;

    let query = "SELECT c, b, SUM(d)
            FROM multiple_ordered_table_with_pk
            GROUP BY c";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_aggregation() -> Result<()> {
    let ctx = SessionContext::new();

    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Decimal128(15, 2), true),
    ]);

    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;

    let query = "SELECT a, SUM(b + 1) as b_sum FROM t1 GROUP BY a ORDER BY b_sum DESC";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_sort() -> Result<()> {
    let ctx = SessionContext::new();

    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Decimal128(15, 2), true),
    ]);

    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;

    let query = "SELECT a, b FROM t1 ORDER BY b LIMIT 5";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_dml() -> Result<()> {
    let ctx = SessionContext::new();
    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Decimal128(15, 2), true),
    ]);

    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;
    let queries = [
        "INSERT INTO T1 VALUES (1, null)",
        "INSERT OVERWRITE T1 VALUES (1, null)",
        "REPLACE INTO T1 VALUES (1, null)",
        "INSERT OR REPLACE INTO T1 VALUES (1, null)",
        "DELETE FROM T1",
        "UPDATE T1 SET a = 1",
        "CREATE TABLE T2 AS SELECT * FROM T1",
    ];
    for query in queries {
        let plan = ctx.sql(query).await?.into_optimized_plan()?;
        let bytes = logical_plan_to_bytes(&plan)?;
        let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
        assert_eq!(
            format!("{plan}"),
            format!("{logical_round_trip}"),
            "failed query roundtrip: {}",
            query
        );
    }

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_sql_options() -> Result<()> {
    let ctx = SessionContext::new();

    let input = create_csv_scan(&ctx).await?;
    let file_type = format_as_file_type(Arc::new(CsvFormatFactory::new()));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.csv".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    let codec = CsvLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_writer_options() -> Result<()> {
    let ctx = SessionContext::new();

    let input = create_csv_scan(&ctx).await?;

    let table_options =
        TableOptions::default_from_session_config(ctx.state().config_options());
    let mut parquet_format = table_options.parquet;

    parquet_format.global.bloom_filter_on_read = true;
    parquet_format.global.created_by = "DataFusion Test".to_string();
    parquet_format.global.writer_version = "PARQUET_2_0".to_string();
    parquet_format.global.write_batch_size = 111;
    parquet_format.global.data_pagesize_limit = 222;
    parquet_format.global.data_page_row_count_limit = 333;
    parquet_format.global.dictionary_page_size_limit = 444;
    parquet_format.global.max_row_group_size = 555;

    let file_type = format_as_file_type(Arc::new(
        ParquetFormatFactory::new_with_options(parquet_format),
    ));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.parquet".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    let codec = ParquetLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan:?}"), format!("{logical_round_trip:?}"));
    match logical_round_trip {
        LogicalPlan::Copy(copy_to) => {
            assert_eq!("test.parquet", copy_to.output_url);
            assert_eq!(vec!["a", "b", "c"], copy_to.partition_by);
            assert_eq!(copy_to.file_type.get_ext(), "parquet".to_string());
        }
        _ => panic!(),
    }
    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_arrow() -> Result<()> {
    let ctx = SessionContext::new();

    let input = create_csv_scan(&ctx).await?;

    let file_type = format_as_file_type(Arc::new(ArrowFormatFactory::new()));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.arrow".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    let codec = ArrowLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    match logical_round_trip {
        LogicalPlan::Copy(copy_to) => {
            assert_eq!("test.arrow", copy_to.output_url);
            assert_eq!("arrow".to_string(), copy_to.file_type.get_ext());
            assert_eq!(vec!["a", "b", "c"], copy_to.partition_by);
        }
        _ => panic!(),
    }

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_csv() -> Result<()> {
    let ctx = SessionContext::new();

    let input = create_csv_scan(&ctx).await?;

    let table_options =
        TableOptions::default_from_session_config(ctx.state().config_options());
    let mut csv_format = table_options.csv;

    csv_format.delimiter = b'*';
    csv_format.date_format = Some("dd/MM/yyyy".to_string());
    csv_format.datetime_format = Some("dd/MM/yyyy HH:mm:ss".to_string());
    csv_format.timestamp_format = Some("HH:mm:ss.SSSSSS".to_string());
    csv_format.time_format = Some("HH:mm:ss".to_string());
    csv_format.null_value = Some("NIL".to_string());

    let file_type = format_as_file_type(Arc::new(CsvFormatFactory::new_with_options(
        csv_format.clone(),
    )));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.csv".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    let codec = CsvLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan:?}"), format!("{logical_round_trip:?}"));

    match logical_round_trip {
        LogicalPlan::Copy(copy_to) => {
            assert_eq!("test.csv", copy_to.output_url);
            assert_eq!("csv".to_string(), copy_to.file_type.get_ext());
            assert_eq!(vec!["a", "b", "c"], copy_to.partition_by);

            let file_type = copy_to
                .file_type
                .as_ref()
                .as_any()
                .downcast_ref::<DefaultFileType>()
                .unwrap();

            let format_factory = file_type.as_format_factory();
            let csv_factory = format_factory
                .as_ref()
                .as_any()
                .downcast_ref::<CsvFormatFactory>()
                .unwrap();
            let csv_config = csv_factory.options.as_ref().unwrap();
            assert_eq!(csv_format.delimiter, csv_config.delimiter);
            assert_eq!(csv_format.date_format, csv_config.date_format);
            assert_eq!(csv_format.datetime_format, csv_config.datetime_format);
            assert_eq!(csv_format.timestamp_format, csv_config.timestamp_format);
            assert_eq!(csv_format.time_format, csv_config.time_format);
            assert_eq!(csv_format.null_value, csv_config.null_value)
        }
        _ => panic!(),
    }

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_json() -> Result<()> {
    let ctx = SessionContext::new();

    // Assume create_json_scan creates a logical plan for scanning a JSON file
    let input = create_json_scan(&ctx).await?;

    let table_options =
        TableOptions::default_from_session_config(ctx.state().config_options());
    let mut json_format = table_options.json;

    // Set specific JSON format options
    json_format.compression = CompressionTypeVariant::GZIP;
    json_format.schema_infer_max_rec = Some(1000);

    let file_type = format_as_file_type(Arc::new(JsonFormatFactory::new_with_options(
        json_format.clone(),
    )));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.json".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    // Assume JsonLogicalExtensionCodec is implemented similarly to CsvLogicalExtensionCodec
    let codec = JsonLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    match logical_round_trip {
        LogicalPlan::Copy(copy_to) => {
            assert_eq!("test.json", copy_to.output_url);
            assert_eq!("json".to_string(), copy_to.file_type.get_ext());
            assert_eq!(vec!["a", "b", "c"], copy_to.partition_by);

            let file_type = copy_to
                .file_type
                .as_ref()
                .as_any()
                .downcast_ref::<DefaultFileType>()
                .unwrap();

            let format_factory = file_type.as_format_factory();
            let json_factory = format_factory
                .as_ref()
                .as_any()
                .downcast_ref::<JsonFormatFactory>()
                .unwrap();
            let json_config = json_factory.options.as_ref().unwrap();
            assert_eq!(json_format.compression, json_config.compression);
            assert_eq!(
                json_format.schema_infer_max_rec,
                json_config.schema_infer_max_rec
            );
        }
        _ => panic!(),
    }

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_copy_to_parquet() -> Result<()> {
    let ctx = SessionContext::new();

    // Assume create_parquet_scan creates a logical plan for scanning a Parquet file
    let input = create_parquet_scan(&ctx).await?;

    let table_options =
        TableOptions::default_from_session_config(ctx.state().config_options());
    let mut parquet_format = table_options.parquet;

    // Set specific Parquet format options
    let mut key_value_metadata = HashMap::new();
    key_value_metadata.insert("test".to_string(), Some("test".to_string()));
    parquet_format
        .key_value_metadata
        .clone_from(&key_value_metadata);

    parquet_format.global.allow_single_file_parallelism = false;
    parquet_format.global.created_by = "test".to_string();

    let file_type = format_as_file_type(Arc::new(
        ParquetFormatFactory::new_with_options(parquet_format.clone()),
    ));

    let plan = LogicalPlan::Copy(CopyTo::new(
        Arc::new(input),
        "test.parquet".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        file_type,
        Default::default(),
    ));

    // Assume ParquetLogicalExtensionCodec is implemented similarly to JsonLogicalExtensionCodec
    let codec = ParquetLogicalExtensionCodec {};
    let bytes = logical_plan_to_bytes_with_extension_codec(&plan, &codec)?;
    let logical_round_trip =
        logical_plan_from_bytes_with_extension_codec(&bytes, &ctx, &codec)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    match logical_round_trip {
        LogicalPlan::Copy(copy_to) => {
            assert_eq!("test.parquet", copy_to.output_url);
            assert_eq!("parquet".to_string(), copy_to.file_type.get_ext());
            assert_eq!(vec!["a", "b", "c"], copy_to.partition_by);

            let file_type = copy_to
                .file_type
                .as_ref()
                .as_any()
                .downcast_ref::<DefaultFileType>()
                .unwrap();

            let format_factory = file_type.as_format_factory();
            let parquet_factory = format_factory
                .as_ref()
                .as_any()
                .downcast_ref::<ParquetFormatFactory>()
                .unwrap();
            let parquet_config = parquet_factory.options.as_ref().unwrap();
            assert_eq!(parquet_config.key_value_metadata, key_value_metadata);
            assert!(!parquet_config.global.allow_single_file_parallelism);
            assert_eq!(parquet_config.global.created_by, "test".to_string());
        }
        _ => panic!(),
    }

    Ok(())
}

async fn create_csv_scan(ctx: &SessionContext) -> Result<LogicalPlan, DataFusionError> {
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;

    let input = ctx.table("t1").await?.into_optimized_plan()?;
    Ok(input)
}

async fn create_json_scan(ctx: &SessionContext) -> Result<LogicalPlan, DataFusionError> {
    ctx.register_json(
        "t1",
        "../core/tests/data/1.json",
        NdJsonReadOptions::default(),
    )
    .await?;

    let input = ctx.table("t1").await?.into_optimized_plan()?;
    Ok(input)
}

async fn create_parquet_scan(
    ctx: &SessionContext,
) -> Result<LogicalPlan, DataFusionError> {
    ctx.register_parquet(
        "t1",
        "../substrait/tests/testdata/empty.parquet",
        ParquetReadOptions::default(),
    )
    .await?;

    let input = ctx.table("t1").await?.into_optimized_plan()?;
    Ok(input)
}

#[tokio::test]
async fn roundtrip_logical_plan_distinct_on() -> Result<()> {
    let ctx = SessionContext::new();

    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Decimal128(15, 2), true),
    ]);

    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;

    let query = "SELECT DISTINCT ON (a % 2) a, b * 2 FROM t1 ORDER BY a % 2 DESC, b";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_single_count_distinct() -> Result<()> {
    let ctx = SessionContext::new();

    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Decimal128(15, 2), true),
    ]);

    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;

    let query = "SELECT a, COUNT(DISTINCT b) as b_cd FROM t1 GROUP BY a";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_with_extension() -> Result<()> {
    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    let plan = ctx.table("t1").await?.into_optimized_plan()?;
    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));
    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_unnest() -> Result<()> {
    let ctx = SessionContext::new();
    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new(
            "b",
            DataType::List(Arc::new(Field::new_list_field(DataType::Int32, false))),
            true,
        ),
    ]);
    ctx.register_csv(
        "t1",
        "tests/testdata/test.csv",
        CsvReadOptions::default().schema(&schema),
    )
    .await?;
    let query = "SELECT unnest(b) FROM t1";
    let plan = ctx.sql(query).await?.into_optimized_plan()?;
    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));
    Ok(())
}

#[tokio::test]
async fn roundtrip_expr_api() -> Result<()> {
    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    let table = ctx.table("t1").await?;
    let schema = table.schema().clone();

    // list of expressions to round trip
    let expr_list = vec![
        encode(col("a").cast_to(&DataType::Utf8, &schema)?, lit("hex")),
        decode(lit("1234"), lit("hex")),
        array_to_string(make_array(vec![lit(1), lit(2), lit(3)]), lit(",")),
        array_dims(make_array(vec![lit(1), lit(2), lit(3)])),
        array_ndims(make_array(vec![lit(1), lit(2), lit(3)])),
        cardinality(make_array(vec![lit(1), lit(2), lit(3)])),
        string_to_array(lit("abc#def#ghl"), lit("#"), lit(",")),
        range(lit(1), lit(10), lit(2)),
        gen_series(lit(1), lit(10), lit(2)),
        array_append(make_array(vec![lit(1), lit(2), lit(3)]), lit(4)),
        array_prepend(lit(1), make_array(vec![lit(2), lit(3), lit(4)])),
        array_concat(vec![
            make_array(vec![lit(1), lit(2)]),
            make_array(vec![lit(3), lit(4)]),
        ]),
        make_array(vec![lit(1), lit(2), lit(3)]),
        array_has(make_array(vec![lit(1), lit(2), lit(3)]), lit(1)),
        array_has_all(
            make_array(vec![lit(1), lit(2), lit(3)]),
            make_array(vec![lit(1), lit(2)]),
        ),
        array_has_any(
            make_array(vec![lit(1), lit(2), lit(3)]),
            make_array(vec![lit(1), lit(4)]),
        ),
        array_empty(make_array(vec![lit(1), lit(2), lit(3)])),
        array_length(make_array(vec![lit(1), lit(2), lit(3)])),
        array_repeat(lit(1), lit(3)),
        flatten(make_array(vec![lit(1), lit(2), lit(3)])),
        array_sort(
            make_array(vec![lit(3), lit(4), lit(1), lit(2)]),
            lit("desc"),
            lit("NULLS LAST"),
        ),
        array_distinct(make_array(vec![lit(1), lit(3), lit(3), lit(2), lit(2)])),
        array_intersect(
            make_array(vec![lit(1), lit(3)]),
            make_array(vec![lit(1), lit(4)]),
        ),
        array_union(
            make_array(vec![lit(1), lit(3)]),
            make_array(vec![lit(1), lit(4)]),
        ),
        array_resize(make_array(vec![lit(1), lit(2), lit(3)]), lit(5), lit(0)),
        array_element(make_array(vec![lit(1), lit(2), lit(3)]), lit(2)),
        array_slice(
            make_array(vec![lit(1), lit(2), lit(3)]),
            lit(1),
            lit(2),
            Some(lit(1)),
        ),
        array_slice(
            make_array(vec![lit(1), lit(2), lit(3)]),
            lit(1),
            lit(2),
            None,
        ),
        array_pop_front(make_array(vec![lit(1), lit(2), lit(3)])),
        array_pop_back(make_array(vec![lit(1), lit(2), lit(3)])),
        array_any_value(make_array(vec![
            lit(ScalarValue::Null),
            lit(1),
            lit(2),
            lit(3),
        ])),
        array_reverse(make_array(vec![lit(1), lit(2), lit(3)])),
        array_position(
            make_array(vec![lit(1), lit(2), lit(3), lit(4)]),
            lit(3),
            lit(2),
        ),
        array_positions(make_array(vec![lit(4), lit(3), lit(3), lit(1)]), lit(3)),
        array_except(
            make_array(vec![lit(1), lit(2), lit(3)]),
            make_array(vec![lit(1), lit(2)]),
        ),
        array_remove(make_array(vec![lit(4), lit(3), lit(2), lit(1)]), lit(3)),
        array_remove_n(
            make_array(vec![lit(1), lit(3), lit(3), lit(3)]),
            lit(3),
            lit(2),
        ),
        array_remove_all(
            make_array(vec![lit(3), lit(3), lit(2), lit(3), lit(1)]),
            lit(3),
        ),
        array_replace(make_array(vec![lit(1), lit(2), lit(3)]), lit(2), lit(4)),
        array_replace_n(
            make_array(vec![lit(1), lit(2), lit(3)]),
            lit(2),
            lit(4),
            lit(1),
        ),
        array_replace_all(make_array(vec![lit(1), lit(2), lit(3)]), lit(2), lit(4)),
        count(lit(1)),
        count_distinct(lit(1)),
        first_value(lit(1), vec![]),
        first_value(lit(1), vec![lit(2).sort(true, true)]),
        functions_window::nth_value::first_value(lit(1)),
        functions_window::nth_value::last_value(lit(1)),
        functions_window::nth_value::nth_value(lit(1), 1),
        avg(lit(1.5)),
        covar_samp(lit(1.5), lit(2.2)),
        covar_pop(lit(1.5), lit(2.2)),
        corr(lit(1.5), lit(2.2)),
        sum(lit(1)),
        max(lit(1)),
        median(lit(2)),
        min(lit(2)),
        var_sample(lit(2.2)),
        var_pop(lit(2.2)),
        stddev(lit(2.2)),
        stddev_pop(lit(2.2)),
        approx_distinct(lit(2)),
        approx_median(lit(2)),
        approx_percentile_cont(lit(2).sort(true, false), lit(0.5), None),
        approx_percentile_cont(lit(2).sort(true, false), lit(0.5), Some(lit(50))),
        approx_percentile_cont_with_weight(lit(2), lit(1), lit(0.5)),
        grouping(lit(1)),
        bit_and(lit(2)),
        bit_or(lit(2)),
        bit_xor(lit(2)),
        string_agg(col("a").cast_to(&DataType::Utf8, &schema)?, lit("|")),
        bool_and(lit(true)),
        bool_or(lit(true)),
        array_agg(lit(1)),
        array_agg(lit(1)).distinct().build().unwrap(),
        map(
            vec![lit(1), lit(2), lit(3)],
            vec![lit(10), lit(20), lit(30)],
        ),
        cume_dist(),
        row_number(),
        rank(),
        dense_rank(),
        percent_rank(),
        lead(col("b"), None, None),
        lead(col("b"), Some(2), None),
        lead(col("b"), Some(2), Some(ScalarValue::from(100))),
        lag(col("b"), None, None),
        lag(col("b"), Some(2), None),
        lag(col("b"), Some(2), Some(ScalarValue::from(100))),
        ntile(lit(3)),
        nth_value(col("b"), 1, vec![]),
        nth_value(
            col("b"),
            1,
            vec![col("a").sort(false, false), col("b").sort(true, false)],
        ),
        nth_value(col("b"), -1, vec![]),
        nth_value(
            col("b"),
            -1,
            vec![col("a").sort(false, false), col("b").sort(true, false)],
        ),
    ];

    // ensure expressions created with the expr api can be round tripped
    let plan = table.select(expr_list)?.into_optimized_plan()?;
    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));
    Ok(())
}

#[tokio::test]
async fn roundtrip_logical_plan_with_view_scan() -> Result<()> {
    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    ctx.sql("CREATE VIEW view_t1(a, b) AS SELECT a, b FROM t1")
        .await?;

    // SELECT
    let plan = ctx
        .sql("SELECT * FROM view_t1")
        .await?
        .into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    // DROP
    let plan = ctx.sql("DROP VIEW view_t1").await?.into_optimized_plan()?;
    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan}"), format!("{logical_round_trip}"));

    Ok(())
}

pub mod proto {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct TopKPlanProto {
        #[prost(uint64, tag = "1")]
        pub k: u64,

        #[prost(message, optional, tag = "2")]
        pub expr: Option<datafusion_proto::protobuf::LogicalExprNode>,
    }

    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct TopKExecProto {
        #[prost(uint64, tag = "1")]
        pub k: u64,
    }
}

#[derive(PartialEq, Eq, PartialOrd, Hash)]
struct TopKPlanNode {
    k: usize,
    input: LogicalPlan,
    /// The sort expression (this example only supports a single sort
    /// expr)
    expr: Expr,
}

impl TopKPlanNode {
    pub fn new(k: usize, input: LogicalPlan, expr: Expr) -> Self {
        Self { k, input, expr }
    }
}

impl Debug for TopKPlanNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_for_explain(f)
    }
}

impl UserDefinedLogicalNodeCore for TopKPlanNode {
    fn name(&self) -> &str {
        "TopK"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }

    /// Schema for TopK is the same as the input
    fn schema(&self) -> &DFSchemaRef {
        self.input.schema()
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![self.expr.clone()]
    }

    /// For example: `TopK: k=10`
    fn fmt_for_explain(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "TopK: k={}", self.k)
    }

    fn with_exprs_and_inputs(
        &self,
        mut exprs: Vec<Expr>,
        mut inputs: Vec<LogicalPlan>,
    ) -> Result<Self> {
        assert_eq!(inputs.len(), 1, "input size inconsistent");
        assert_eq!(exprs.len(), 1, "expression size inconsistent");
        Ok(Self {
            k: self.k,
            input: inputs.swap_remove(0),
            expr: exprs.swap_remove(0),
        })
    }

    fn supports_limit_pushdown(&self) -> bool {
        false // Disallow limit push-down by default
    }
}

#[derive(Debug)]
pub struct TopKExtensionCodec {}

impl LogicalExtensionCodec for TopKExtensionCodec {
    fn try_decode(
        &self,
        buf: &[u8],
        inputs: &[LogicalPlan],
        ctx: &SessionContext,
    ) -> Result<Extension> {
        if let Some((input, _)) = inputs.split_first() {
            let proto = proto::TopKPlanProto::decode(buf).map_err(|e| {
                DataFusionError::Internal(format!("failed to decode logical plan: {e:?}"))
            })?;

            if let Some(expr) = proto.expr.as_ref() {
                let node = TopKPlanNode::new(
                    proto.k as usize,
                    input.clone(),
                    from_proto::parse_expr(expr, ctx, self)?,
                );

                Ok(Extension {
                    node: Arc::new(node),
                })
            } else {
                internal_err!("invalid plan, no expr")
            }
        } else {
            internal_err!("invalid plan, no input")
        }
    }

    fn try_encode(&self, node: &Extension, buf: &mut Vec<u8>) -> Result<()> {
        if let Some(exec) = node.node.as_any().downcast_ref::<TopKPlanNode>() {
            let proto = proto::TopKPlanProto {
                k: exec.k as u64,
                expr: Some(serialize_expr(&exec.expr, self)?),
            };

            proto.encode(buf).map_err(|e| {
                DataFusionError::Internal(format!("failed to encode logical plan: {e:?}"))
            })?;

            Ok(())
        } else {
            internal_err!("unsupported plan type")
        }
    }

    fn try_decode_table_provider(
        &self,
        _buf: &[u8],
        _table_ref: &TableReference,
        _schema: SchemaRef,
        _ctx: &SessionContext,
    ) -> Result<Arc<dyn TableProvider>> {
        internal_err!("unsupported plan type")
    }

    fn try_encode_table_provider(
        &self,
        _table_ref: &TableReference,
        _node: Arc<dyn TableProvider>,
        _buf: &mut Vec<u8>,
    ) -> Result<()> {
        internal_err!("unsupported plan type")
    }
}

#[derive(Debug)]
pub struct UDFExtensionCodec;

impl LogicalExtensionCodec for UDFExtensionCodec {
    fn try_decode(
        &self,
        _buf: &[u8],
        _inputs: &[LogicalPlan],
        _ctx: &SessionContext,
    ) -> Result<Extension> {
        not_impl_err!("No extension codec provided")
    }

    fn try_encode(&self, _node: &Extension, _buf: &mut Vec<u8>) -> Result<()> {
        not_impl_err!("No extension codec provided")
    }

    fn try_decode_table_provider(
        &self,
        _buf: &[u8],
        _table_ref: &TableReference,
        _schema: SchemaRef,
        _ctx: &SessionContext,
    ) -> Result<Arc<dyn TableProvider>> {
        internal_err!("unsupported plan type")
    }

    fn try_encode_table_provider(
        &self,
        _table_ref: &TableReference,
        _node: Arc<dyn TableProvider>,
        _buf: &mut Vec<u8>,
    ) -> Result<()> {
        internal_err!("unsupported plan type")
    }

    fn try_decode_udf(&self, name: &str, buf: &[u8]) -> Result<Arc<ScalarUDF>> {
        if name == "regex_udf" {
            let proto = MyRegexUdfNode::decode(buf).map_err(|err| {
                DataFusionError::Internal(format!("failed to decode regex_udf: {err}"))
            })?;

            Ok(Arc::new(ScalarUDF::from(MyRegexUdf::new(proto.pattern))))
        } else {
            not_impl_err!("unrecognized scalar UDF implementation, cannot decode")
        }
    }

    fn try_encode_udf(&self, node: &ScalarUDF, buf: &mut Vec<u8>) -> Result<()> {
        let binding = node.inner();
        let udf = binding.as_any().downcast_ref::<MyRegexUdf>().unwrap();
        let proto = MyRegexUdfNode {
            pattern: udf.pattern.clone(),
        };
        proto.encode(buf).map_err(|err| {
            DataFusionError::Internal(format!("failed to encode udf: {err}"))
        })?;
        Ok(())
    }

    fn try_decode_udaf(&self, name: &str, buf: &[u8]) -> Result<Arc<AggregateUDF>> {
        if name == "aggregate_udf" {
            let proto = MyAggregateUdfNode::decode(buf).map_err(|err| {
                DataFusionError::Internal(format!(
                    "failed to decode aggregate_udf: {err}"
                ))
            })?;

            Ok(Arc::new(AggregateUDF::from(MyAggregateUDF::new(
                proto.result,
            ))))
        } else {
            not_impl_err!("unrecognized aggregate UDF implementation, cannot decode")
        }
    }

    fn try_encode_udaf(&self, node: &AggregateUDF, buf: &mut Vec<u8>) -> Result<()> {
        let binding = node.inner();
        let udf = binding.as_any().downcast_ref::<MyAggregateUDF>().unwrap();
        let proto = MyAggregateUdfNode {
            result: udf.result.clone(),
        };
        proto.encode(buf).map_err(|err| {
            DataFusionError::Internal(format!("failed to encode udf: {err}"))
        })?;
        Ok(())
    }
}

#[test]
fn round_trip_scalar_values_and_data_types() {
    let should_pass: Vec<ScalarValue> = vec![
        ScalarValue::Boolean(None),
        ScalarValue::Float32(None),
        ScalarValue::Float64(None),
        ScalarValue::Int8(None),
        ScalarValue::Int16(None),
        ScalarValue::Int32(None),
        ScalarValue::Int64(None),
        ScalarValue::UInt8(None),
        ScalarValue::UInt16(None),
        ScalarValue::UInt32(None),
        ScalarValue::UInt64(None),
        ScalarValue::Utf8(None),
        ScalarValue::LargeUtf8(None),
        ScalarValue::List(ScalarValue::new_list_nullable(&[], &DataType::Boolean)),
        ScalarValue::LargeList(ScalarValue::new_large_list(&[], &DataType::Boolean)),
        ScalarValue::Date32(None),
        ScalarValue::Boolean(Some(true)),
        ScalarValue::Boolean(Some(false)),
        ScalarValue::Float32(Some(1.0)),
        ScalarValue::Float32(Some(f32::MAX)),
        ScalarValue::Float32(Some(f32::MIN)),
        ScalarValue::Float32(Some(-2000.0)),
        ScalarValue::Float64(Some(1.0)),
        ScalarValue::Float64(Some(f64::MAX)),
        ScalarValue::Float64(Some(f64::MIN)),
        ScalarValue::Float64(Some(-2000.0)),
        ScalarValue::Int8(Some(i8::MIN)),
        ScalarValue::Int8(Some(i8::MAX)),
        ScalarValue::Int8(Some(0)),
        ScalarValue::Int8(Some(-15)),
        ScalarValue::Int16(Some(i16::MIN)),
        ScalarValue::Int16(Some(i16::MAX)),
        ScalarValue::Int16(Some(0)),
        ScalarValue::Int16(Some(-15)),
        ScalarValue::Int32(Some(i32::MIN)),
        ScalarValue::Int32(Some(i32::MAX)),
        ScalarValue::Int32(Some(0)),
        ScalarValue::Int32(Some(-15)),
        ScalarValue::Int64(Some(i64::MIN)),
        ScalarValue::Int64(Some(i64::MAX)),
        ScalarValue::Int64(Some(0)),
        ScalarValue::Int64(Some(-15)),
        ScalarValue::UInt8(Some(u8::MAX)),
        ScalarValue::UInt8(Some(0)),
        ScalarValue::UInt16(Some(u16::MAX)),
        ScalarValue::UInt16(Some(0)),
        ScalarValue::UInt32(Some(u32::MAX)),
        ScalarValue::UInt32(Some(0)),
        ScalarValue::UInt64(Some(u64::MAX)),
        ScalarValue::UInt64(Some(0)),
        ScalarValue::Utf8(Some(String::from("Test string   "))),
        ScalarValue::LargeUtf8(Some(String::from("Test Large utf8"))),
        ScalarValue::Utf8View(Some(String::from("Test stringview"))),
        ScalarValue::BinaryView(Some(b"binaryview".to_vec())),
        ScalarValue::Date32(Some(0)),
        ScalarValue::Date32(Some(i32::MAX)),
        ScalarValue::Date32(None),
        ScalarValue::Date64(Some(0)),
        ScalarValue::Date64(Some(i64::MAX)),
        ScalarValue::Date64(None),
        ScalarValue::Time32Second(Some(0)),
        ScalarValue::Time32Second(Some(i32::MAX)),
        ScalarValue::Time32Second(None),
        ScalarValue::Time32Millisecond(Some(0)),
        ScalarValue::Time32Millisecond(Some(i32::MAX)),
        ScalarValue::Time32Millisecond(None),
        ScalarValue::Time64Microsecond(Some(0)),
        ScalarValue::Time64Microsecond(Some(i64::MAX)),
        ScalarValue::Time64Microsecond(None),
        ScalarValue::Time64Nanosecond(Some(0)),
        ScalarValue::Time64Nanosecond(Some(i64::MAX)),
        ScalarValue::Time64Nanosecond(None),
        ScalarValue::TimestampNanosecond(Some(0), None),
        ScalarValue::TimestampNanosecond(Some(i64::MAX), None),
        ScalarValue::TimestampNanosecond(Some(0), Some("UTC".into())),
        ScalarValue::TimestampNanosecond(None, None),
        ScalarValue::TimestampMicrosecond(Some(0), None),
        ScalarValue::TimestampMicrosecond(Some(i64::MAX), None),
        ScalarValue::TimestampMicrosecond(Some(0), Some("UTC".into())),
        ScalarValue::TimestampMicrosecond(None, None),
        ScalarValue::TimestampMillisecond(Some(0), None),
        ScalarValue::TimestampMillisecond(Some(i64::MAX), None),
        ScalarValue::TimestampMillisecond(Some(0), Some("UTC".into())),
        ScalarValue::TimestampMillisecond(None, None),
        ScalarValue::TimestampSecond(Some(0), None),
        ScalarValue::TimestampSecond(Some(i64::MAX), None),
        ScalarValue::TimestampSecond(Some(0), Some("UTC".into())),
        ScalarValue::TimestampSecond(None, None),
        ScalarValue::IntervalDayTime(Some(IntervalDayTimeType::make_value(0, 0))),
        ScalarValue::IntervalDayTime(Some(IntervalDayTimeType::make_value(1, 2))),
        ScalarValue::IntervalDayTime(Some(IntervalDayTimeType::make_value(
            i32::MAX,
            i32::MAX,
        ))),
        ScalarValue::IntervalDayTime(None),
        ScalarValue::IntervalMonthDayNano(Some(IntervalMonthDayNanoType::make_value(
            0, 0, 0,
        ))),
        ScalarValue::IntervalMonthDayNano(Some(IntervalMonthDayNanoType::make_value(
            1, 2, 3,
        ))),
        ScalarValue::IntervalMonthDayNano(Some(IntervalMonthDayNanoType::make_value(
            i32::MAX,
            i32::MAX,
            i64::MAX,
        ))),
        ScalarValue::IntervalMonthDayNano(None),
        ScalarValue::List(ScalarValue::new_list_nullable(
            &[
                ScalarValue::Float32(Some(-213.1)),
                ScalarValue::Float32(None),
                ScalarValue::Float32(Some(5.5)),
                ScalarValue::Float32(Some(2.0)),
                ScalarValue::Float32(Some(1.0)),
            ],
            &DataType::Float32,
        )),
        ScalarValue::LargeList(ScalarValue::new_large_list(
            &[
                ScalarValue::Float32(Some(-213.1)),
                ScalarValue::Float32(None),
                ScalarValue::Float32(Some(5.5)),
                ScalarValue::Float32(Some(2.0)),
                ScalarValue::Float32(Some(1.0)),
            ],
            &DataType::Float32,
        )),
        ScalarValue::List(ScalarValue::new_list_nullable(
            &[
                ScalarValue::List(ScalarValue::new_list_nullable(
                    &[],
                    &DataType::Float32,
                )),
                ScalarValue::List(ScalarValue::new_list_nullable(
                    &[
                        ScalarValue::Float32(Some(-213.1)),
                        ScalarValue::Float32(None),
                        ScalarValue::Float32(Some(5.5)),
                        ScalarValue::Float32(Some(2.0)),
                        ScalarValue::Float32(Some(1.0)),
                    ],
                    &DataType::Float32,
                )),
            ],
            &DataType::List(new_arc_field("item", DataType::Float32, true)),
        )),
        ScalarValue::LargeList(ScalarValue::new_large_list(
            &[
                ScalarValue::LargeList(ScalarValue::new_large_list(
                    &[],
                    &DataType::Float32,
                )),
                ScalarValue::LargeList(ScalarValue::new_large_list(
                    &[
                        ScalarValue::Float32(Some(-213.1)),
                        ScalarValue::Float32(None),
                        ScalarValue::Float32(Some(5.5)),
                        ScalarValue::Float32(Some(2.0)),
                        ScalarValue::Float32(Some(1.0)),
                    ],
                    &DataType::Float32,
                )),
            ],
            &DataType::LargeList(new_arc_field("item", DataType::Float32, true)),
        )),
        ScalarValue::FixedSizeList(Arc::new(FixedSizeListArray::from_iter_primitive::<
            Int32Type,
            _,
            _,
        >(
            vec![Some(vec![Some(1), Some(2), Some(3)])],
            3,
        ))),
        ScalarValue::Dictionary(
            Box::new(DataType::Int32),
            Box::new(ScalarValue::from("foo")),
        ),
        ScalarValue::Dictionary(
            Box::new(DataType::Int32),
            Box::new(ScalarValue::Utf8(None)),
        ),
        ScalarValue::Binary(Some(b"bar".to_vec())),
        ScalarValue::Binary(None),
        ScalarValue::LargeBinary(Some(b"bar".to_vec())),
        ScalarValue::LargeBinary(None),
        ScalarStructBuilder::new()
            .with_scalar(
                Field::new("a", DataType::Int32, true),
                ScalarValue::from(23i32),
            )
            .with_scalar(
                Field::new("b", DataType::Boolean, false),
                ScalarValue::from(false),
            )
            .build()
            .unwrap(),
        ScalarStructBuilder::new()
            .with_scalar(
                Field::new("a", DataType::Int32, true),
                ScalarValue::from(23i32),
            )
            .with_scalar(
                Field::new("b", DataType::Boolean, false),
                ScalarValue::from(false),
            )
            .build()
            .unwrap(),
        ScalarValue::try_from(&DataType::Struct(Fields::from(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Boolean, false),
        ])))
        .unwrap(),
        ScalarValue::try_from(&DataType::Struct(Fields::from(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Boolean, false),
        ])))
        .unwrap(),
        ScalarValue::try_from(&DataType::Map(
            Arc::new(Field::new(
                "entries",
                DataType::Struct(Fields::from(vec![
                    Field::new("key", DataType::Int32, true),
                    Field::new("value", DataType::Utf8, false),
                ])),
                false,
            )),
            false,
        ))
        .unwrap(),
        ScalarValue::try_from(&DataType::Map(
            Arc::new(Field::new(
                "entries",
                DataType::Struct(Fields::from(vec![
                    Field::new("key", DataType::Int32, true),
                    Field::new("value", DataType::Utf8, true),
                ])),
                false,
            )),
            true,
        ))
        .unwrap(),
        ScalarValue::Map(Arc::new(create_map_array_test_case())),
        ScalarValue::FixedSizeBinary(b"bar".to_vec().len() as i32, Some(b"bar".to_vec())),
        ScalarValue::FixedSizeBinary(0, None),
        ScalarValue::FixedSizeBinary(5, None),
    ];

    // ScalarValue directly
    for test_case in should_pass.iter() {
        let proto: protobuf::ScalarValue =
            test_case.try_into().expect("failed conversion to protobuf");
        let roundtrip: ScalarValue = (&proto)
            .try_into()
            .expect("failed conversion from protobuf");

        assert_eq!(
            test_case, &roundtrip,
            "ScalarValue was not the same after round trip!\n\n\
                        Input: {test_case:?}\n\nRoundtrip: {roundtrip:?}"
        );
    }

    //  DataType conversion
    for test_case in should_pass.iter() {
        let dt = test_case.data_type();

        let proto: protobuf::ArrowType = (&dt)
            .try_into()
            .expect("datatype failed conversion to protobuf");
        let roundtrip: DataType = (&proto)
            .try_into()
            .expect("datatype failed conversion from protobuf");

        assert_eq!(
            dt, roundtrip,
            "DataType was not the same after round trip!\n\n\
                        Input: {dt:?}\n\nRoundtrip: {roundtrip:?}"
        );
    }
}

// create a map array [{joe:1}, {blogs:2, foo:4}, {}, null] for testing
fn create_map_array_test_case() -> MapArray {
    let string_builder = StringBuilder::new();
    let int_builder = Int32Builder::with_capacity(4);
    let mut builder = MapBuilder::new(None, string_builder, int_builder);
    builder.keys().append_value("joe");
    builder.values().append_value(1);
    builder.append(true).unwrap();

    builder.keys().append_value("blogs");
    builder.values().append_value(2);
    builder.keys().append_value("foo");
    builder.values().append_value(4);
    builder.append(true).unwrap();
    builder.append(true).unwrap();
    builder.append(false).unwrap();
    builder.finish()
}

#[test]
fn round_trip_scalar_types() {
    let should_pass: Vec<DataType> = vec![
        DataType::Boolean,
        DataType::Int8,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::UInt8,
        DataType::UInt16,
        DataType::UInt32,
        DataType::UInt64,
        DataType::Float32,
        DataType::Float64,
        DataType::Date32,
        DataType::Time64(TimeUnit::Microsecond),
        DataType::Time64(TimeUnit::Nanosecond),
        DataType::Utf8,
        DataType::LargeUtf8,
        // Recursive list tests
        DataType::List(new_arc_field("level1", DataType::Boolean, true)),
        DataType::List(new_arc_field(
            "Level1",
            DataType::List(new_arc_field("level2", DataType::Date32, true)),
            true,
        )),
    ];

    for test_case in should_pass.into_iter() {
        let field = Field::new_list_field(test_case, true);
        let proto: protobuf::Field = (&field).try_into().unwrap();
        let roundtrip: Field = (&proto).try_into().unwrap();
        assert_eq!(format!("{field:?}"), format!("{roundtrip:?}"));
    }
}

#[test]
fn round_trip_datatype() {
    let test_cases: Vec<DataType> = vec![
        DataType::Null,
        DataType::Boolean,
        DataType::Int8,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::UInt8,
        DataType::UInt16,
        DataType::UInt32,
        DataType::UInt64,
        DataType::Float16,
        DataType::Float32,
        DataType::Float64,
        DataType::Timestamp(TimeUnit::Second, None),
        DataType::Timestamp(TimeUnit::Millisecond, None),
        DataType::Timestamp(TimeUnit::Microsecond, None),
        DataType::Timestamp(TimeUnit::Nanosecond, None),
        DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
        DataType::Date32,
        DataType::Date64,
        DataType::Time32(TimeUnit::Second),
        DataType::Time32(TimeUnit::Millisecond),
        DataType::Time32(TimeUnit::Microsecond),
        DataType::Time32(TimeUnit::Nanosecond),
        DataType::Time64(TimeUnit::Second),
        DataType::Time64(TimeUnit::Millisecond),
        DataType::Time64(TimeUnit::Microsecond),
        DataType::Time64(TimeUnit::Nanosecond),
        DataType::Duration(TimeUnit::Second),
        DataType::Duration(TimeUnit::Millisecond),
        DataType::Duration(TimeUnit::Microsecond),
        DataType::Duration(TimeUnit::Nanosecond),
        DataType::Interval(IntervalUnit::YearMonth),
        DataType::Interval(IntervalUnit::DayTime),
        DataType::Binary,
        DataType::FixedSizeBinary(0),
        DataType::FixedSizeBinary(1234),
        DataType::FixedSizeBinary(-432),
        DataType::LargeBinary,
        DataType::Utf8,
        DataType::LargeUtf8,
        DataType::Decimal128(7, 12),
        DataType::Decimal256(DECIMAL256_MAX_PRECISION, 0),
        // Recursive list tests
        DataType::List(new_arc_field("Level1", DataType::Binary, true)),
        DataType::List(new_arc_field(
            "Level1",
            DataType::List(new_arc_field(
                "Level2",
                DataType::FixedSizeBinary(53),
                false,
            )),
            true,
        )),
        // Fixed size lists
        DataType::FixedSizeList(new_arc_field("Level1", DataType::Binary, true), 4),
        DataType::FixedSizeList(
            new_arc_field(
                "Level1",
                DataType::List(new_arc_field(
                    "Level2",
                    DataType::FixedSizeBinary(53),
                    false,
                )),
                true,
            ),
            41,
        ),
        // Struct Testing
        DataType::Struct(Fields::from(vec![
            Field::new("nullable", DataType::Boolean, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("datatype", DataType::Binary, false),
        ])),
        DataType::Struct(Fields::from(vec![
            Field::new("nullable", DataType::Boolean, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("datatype", DataType::Binary, false),
            Field::new(
                "nested_struct",
                DataType::Struct(Fields::from(vec![
                    Field::new("nullable", DataType::Boolean, false),
                    Field::new("name", DataType::Utf8, false),
                    Field::new("datatype", DataType::Binary, false),
                ])),
                true,
            ),
        ])),
        DataType::Union(
            UnionFields::new(
                vec![7, 5, 3],
                vec![
                    Field::new("nullable", DataType::Boolean, false),
                    Field::new("name", DataType::Utf8, false),
                    Field::new("datatype", DataType::Binary, false),
                ],
            ),
            UnionMode::Sparse,
        ),
        DataType::Union(
            UnionFields::new(
                vec![5, 8, 1],
                vec![
                    Field::new("nullable", DataType::Boolean, false),
                    Field::new("name", DataType::Utf8, false),
                    Field::new("datatype", DataType::Binary, false),
                    Field::new_struct(
                        "nested_struct",
                        vec![
                            Field::new("nullable", DataType::Boolean, false),
                            Field::new("name", DataType::Utf8, false),
                            Field::new("datatype", DataType::Binary, false),
                        ],
                        true,
                    ),
                ],
            ),
            UnionMode::Dense,
        ),
        DataType::Dictionary(
            Box::new(DataType::Utf8),
            Box::new(DataType::Struct(Fields::from(vec![
                Field::new("nullable", DataType::Boolean, false),
                Field::new("name", DataType::Utf8, false),
                Field::new("datatype", DataType::Binary, false),
            ]))),
        ),
        DataType::Dictionary(
            Box::new(DataType::Decimal128(10, 50)),
            Box::new(DataType::FixedSizeList(
                new_arc_field("Level1", DataType::Binary, true),
                4,
            )),
        ),
        DataType::Map(
            new_arc_field(
                "entries",
                DataType::Struct(Fields::from(vec![
                    Field::new("keys", DataType::Utf8, false),
                    Field::new("values", DataType::Int32, true),
                ])),
                true,
            ),
            false,
        ),
    ];

    for test_case in test_cases.into_iter() {
        let proto: protobuf::ArrowType = (&test_case).try_into().unwrap();
        let roundtrip: DataType = (&proto).try_into().unwrap();
        assert_eq!(format!("{test_case:?}"), format!("{roundtrip:?}"));
    }
}

#[test]
fn roundtrip_null_scalar_values() {
    let test_types = vec![
        ScalarValue::Boolean(None),
        ScalarValue::Float32(None),
        ScalarValue::Float64(None),
        ScalarValue::Int8(None),
        ScalarValue::Int16(None),
        ScalarValue::Int32(None),
        ScalarValue::Int64(None),
        ScalarValue::UInt8(None),
        ScalarValue::UInt16(None),
        ScalarValue::UInt32(None),
        ScalarValue::UInt64(None),
        ScalarValue::Utf8(None),
        ScalarValue::LargeUtf8(None),
        ScalarValue::Date32(None),
        ScalarValue::TimestampMicrosecond(None, None),
        ScalarValue::TimestampNanosecond(None, None),
    ];

    for test_case in test_types.into_iter() {
        let proto_scalar: protobuf::ScalarValue = (&test_case).try_into().unwrap();
        let returned_scalar: ScalarValue = (&proto_scalar).try_into().unwrap();
        assert_eq!(format!("{:?}", &test_case), format!("{returned_scalar:?}"));
    }
}

#[test]
fn roundtrip_field() {
    let field = Field::new("f", DataType::Int32, true).with_metadata(HashMap::from([
        (String::from("k1"), String::from("v1")),
        (String::from("k2"), String::from("v2")),
    ]));
    let proto_field: protobuf::Field = (&field).try_into().unwrap();
    let returned_field: Field = (&proto_field).try_into().unwrap();
    assert_eq!(field, returned_field);
}

#[test]
fn roundtrip_schema() {
    let schema = Schema::new_with_metadata(
        vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Decimal128(15, 2), true)
                .with_metadata(HashMap::from([(String::from("k1"), String::from("v1"))])),
        ],
        HashMap::from([
            (String::from("k2"), String::from("v2")),
            (String::from("k3"), String::from("v3")),
        ]),
    );
    let proto_schema: protobuf::Schema = (&schema).try_into().unwrap();
    let returned_schema: Schema = (&proto_schema).try_into().unwrap();
    assert_eq!(schema, returned_schema);
}

#[test]
fn roundtrip_dfschema() {
    let dfschema = DFSchema::new_with_metadata(
        vec![
            (None, Arc::new(Field::new("a", DataType::Int64, false))),
            (
                Some("t".into()),
                Arc::new(
                    Field::new("b", DataType::Decimal128(15, 2), true).with_metadata(
                        HashMap::from([(String::from("k1"), String::from("v1"))]),
                    ),
                ),
            ),
        ],
        HashMap::from([
            (String::from("k2"), String::from("v2")),
            (String::from("k3"), String::from("v3")),
        ]),
    )
    .unwrap();
    let proto_dfschema: protobuf::DfSchema = (&dfschema).try_into().unwrap();
    let returned_dfschema: DFSchema = (&proto_dfschema).try_into().unwrap();
    assert_eq!(dfschema, returned_dfschema);

    let arc_dfschema = Arc::new(dfschema.clone());
    let proto_dfschema: protobuf::DfSchema = (&arc_dfschema).try_into().unwrap();
    let returned_arc_dfschema: DFSchemaRef = proto_dfschema.try_into().unwrap();
    assert_eq!(arc_dfschema, returned_arc_dfschema);
    assert_eq!(dfschema, *returned_arc_dfschema);
}

#[test]
fn roundtrip_not() {
    let test_expr = Expr::Not(Box::new(lit(1.0_f32)));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_is_null() {
    let test_expr = Expr::IsNull(Box::new(col("id")));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_is_not_null() {
    let test_expr = Expr::IsNotNull(Box::new(col("id")));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_between() {
    let test_expr = Expr::Between(Between::new(
        Box::new(lit(1.0_f32)),
        true,
        Box::new(lit(2.0_f32)),
        Box::new(lit(3.0_f32)),
    ));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_binary_op() {
    fn test(op: Operator) {
        let test_expr = Expr::BinaryExpr(BinaryExpr::new(
            Box::new(lit(1.0_f32)),
            op,
            Box::new(lit(2.0_f32)),
        ));
        let ctx = SessionContext::new();
        roundtrip_expr_test(test_expr, ctx);
    }
    test(Operator::ArrowAt);
    test(Operator::AtArrow);
    test(Operator::StringConcat);
    test(Operator::RegexNotIMatch);
    test(Operator::RegexNotMatch);
    test(Operator::RegexIMatch);
    test(Operator::RegexMatch);
    test(Operator::BitwiseShiftRight);
    test(Operator::BitwiseShiftLeft);
    test(Operator::BitwiseAnd);
    test(Operator::BitwiseOr);
    test(Operator::BitwiseXor);
    test(Operator::IsDistinctFrom);
    test(Operator::IsNotDistinctFrom);
    test(Operator::And);
    test(Operator::Or);
    test(Operator::Eq);
    test(Operator::NotEq);
    test(Operator::Lt);
    test(Operator::LtEq);
    test(Operator::Gt);
    test(Operator::GtEq);
}

#[test]
fn roundtrip_case() {
    let test_expr = Expr::Case(Case::new(
        Some(Box::new(lit(1.0_f32))),
        vec![(Box::new(lit(2.0_f32)), Box::new(lit(3.0_f32)))],
        Some(Box::new(lit(4.0_f32))),
    ));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_case_with_null() {
    let test_expr = Expr::Case(Case::new(
        Some(Box::new(lit(1.0_f32))),
        vec![(Box::new(lit(2.0_f32)), Box::new(lit(3.0_f32)))],
        Some(Box::new(Expr::Literal(ScalarValue::Null, None))),
    ));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_null_literal() {
    let test_expr = Expr::Literal(ScalarValue::Null, None);

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_cast() {
    let test_expr = Expr::Cast(Cast::new(Box::new(lit(1.0_f32)), DataType::Boolean));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_try_cast() {
    let test_expr =
        Expr::TryCast(TryCast::new(Box::new(lit(1.0_f32)), DataType::Boolean));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);

    let test_expr =
        Expr::TryCast(TryCast::new(Box::new(lit("not a bool")), DataType::Boolean));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_negative() {
    let test_expr = Expr::Negative(Box::new(lit(1.0_f32)));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_inlist() {
    let test_expr = Expr::InList(InList::new(
        Box::new(lit(1.0_f32)),
        vec![lit(2.0_f32)],
        true,
    ));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_unnest() {
    let test_expr = Expr::Unnest(Unnest {
        expr: Box::new(col("col")),
    });

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_wildcard() {
    #[expect(deprecated)]
    let test_expr = Expr::Wildcard {
        qualifier: None,
        options: Box::new(WildcardOptions::default()),
    };

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_qualified_wildcard() {
    #[expect(deprecated)]
    let test_expr = Expr::Wildcard {
        qualifier: Some("foo".into()),
        options: Box::new(WildcardOptions::default()),
    };

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_like() {
    fn like(negated: bool, escape_char: Option<char>) {
        let test_expr = Expr::Like(Like::new(
            negated,
            Box::new(col("col")),
            Box::new(lit("[0-9]+")),
            escape_char,
            false,
        ));
        let ctx = SessionContext::new();
        roundtrip_expr_test(test_expr, ctx);
    }
    like(true, Some('X'));
    like(false, Some('\\'));
    like(true, None);
    like(false, None);
}

#[test]
fn roundtrip_ilike() {
    fn ilike(negated: bool, escape_char: Option<char>) {
        let test_expr = Expr::Like(Like::new(
            negated,
            Box::new(col("col")),
            Box::new(lit("[0-9]+")),
            escape_char,
            true,
        ));
        let ctx = SessionContext::new();
        roundtrip_expr_test(test_expr, ctx);
    }
    ilike(true, Some('X'));
    ilike(false, Some('\\'));
    ilike(true, None);
    ilike(false, None);
}

#[test]
fn roundtrip_similar_to() {
    fn similar_to(negated: bool, escape_char: Option<char>) {
        let test_expr = Expr::SimilarTo(Like::new(
            negated,
            Box::new(col("col")),
            Box::new(lit("[0-9]+")),
            escape_char,
            false,
        ));
        let ctx = SessionContext::new();
        roundtrip_expr_test(test_expr, ctx);
    }
    similar_to(true, Some('X'));
    similar_to(false, Some('\\'));
    similar_to(true, None);
    similar_to(false, None);
}

#[test]
fn roundtrip_count() {
    let test_expr = count(col("bananas"));
    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_count_distinct() {
    let test_expr = count_udaf()
        .call(vec![col("bananas")])
        .distinct()
        .build()
        .unwrap();
    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_aggregate_udf() {
    #[derive(Debug)]
    struct Dummy {}

    impl Accumulator for Dummy {
        fn state(&mut self) -> Result<Vec<ScalarValue>> {
            Ok(vec![])
        }

        fn update_batch(&mut self, _values: &[ArrayRef]) -> Result<()> {
            Ok(())
        }

        fn merge_batch(&mut self, _states: &[ArrayRef]) -> Result<()> {
            Ok(())
        }

        fn evaluate(&mut self) -> Result<ScalarValue> {
            Ok(ScalarValue::Float64(None))
        }

        fn size(&self) -> usize {
            size_of_val(self)
        }
    }

    let dummy_agg = create_udaf(
        // the name; used to represent it in plan descriptions and in the registry, to use in SQL.
        "dummy_agg",
        // the input type; DataFusion guarantees that the first entry of `values` in `update` has this type.
        vec![DataType::Float64],
        // the return type; DataFusion expects this to match the type returned by `evaluate`.
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        // This is the accumulator factory; DataFusion uses it to create new accumulators.
        Arc::new(|_| Ok(Box::new(Dummy {}))),
        // This is the description of the state. `state()` must match the types here.
        Arc::new(vec![DataType::Float64, DataType::UInt32]),
    );

    let test_expr = Expr::AggregateFunction(expr::AggregateFunction::new_udf(
        Arc::new(dummy_agg.clone()),
        vec![lit(1.0_f64)],
        false,
        Some(Box::new(lit(true))),
        vec![],
        None,
    ));

    let ctx = SessionContext::new();
    ctx.register_udaf(dummy_agg);

    roundtrip_expr_test(test_expr, ctx);
}

fn dummy_udf() -> ScalarUDF {
    let scalar_fn = Arc::new(|args: &[ColumnarValue]| {
        let ColumnarValue::Array(array) = &args[0] else {
            panic!("should be array")
        };
        Ok(ColumnarValue::from(Arc::new(array.clone()) as ArrayRef))
    });

    create_udf(
        "dummy",
        vec![DataType::Utf8],
        DataType::Utf8,
        Volatility::Immutable,
        scalar_fn,
    )
}

#[test]
fn roundtrip_scalar_udf() {
    let udf = dummy_udf();

    let test_expr = Expr::ScalarFunction(ScalarFunction::new_udf(
        Arc::new(udf.clone()),
        vec![lit("")],
    ));

    let ctx = SessionContext::new();
    ctx.register_udf(udf);

    roundtrip_expr_test(test_expr.clone(), ctx);

    // Now test loading the UDF without registering it in the context, but rather creating it in the
    // extension codec.
    #[derive(Debug)]
    struct DummyUDFExtensionCodec;

    impl LogicalExtensionCodec for DummyUDFExtensionCodec {
        fn try_decode(
            &self,
            _buf: &[u8],
            _inputs: &[LogicalPlan],
            _ctx: &SessionContext,
        ) -> Result<Extension> {
            not_impl_err!("LogicalExtensionCodec is not provided")
        }

        fn try_encode(&self, _node: &Extension, _buf: &mut Vec<u8>) -> Result<()> {
            not_impl_err!("LogicalExtensionCodec is not provided")
        }

        fn try_decode_table_provider(
            &self,
            _buf: &[u8],
            _table_ref: &TableReference,
            _schema: SchemaRef,
            _ctx: &SessionContext,
        ) -> Result<Arc<dyn TableProvider>> {
            not_impl_err!("LogicalExtensionCodec is not provided")
        }

        fn try_encode_table_provider(
            &self,
            _table_ref: &TableReference,
            _node: Arc<dyn TableProvider>,
            _buf: &mut Vec<u8>,
        ) -> Result<()> {
            not_impl_err!("LogicalExtensionCodec is not provided")
        }

        fn try_decode_udf(&self, name: &str, _buf: &[u8]) -> Result<Arc<ScalarUDF>> {
            if name == "dummy" {
                Ok(Arc::new(dummy_udf()))
            } else {
                Err(DataFusionError::Internal(format!("UDF {name} not found")))
            }
        }
    }

    let ctx = SessionContext::new();
    roundtrip_expr_test_with_codec(test_expr, ctx, &DummyUDFExtensionCodec)
}

#[test]
fn roundtrip_scalar_udf_extension_codec() {
    let udf = ScalarUDF::from(MyRegexUdf::new(".*".to_owned()));
    let test_expr = udf.call(vec!["foo".lit()]);
    let ctx = SessionContext::new();
    let proto = serialize_expr(&test_expr, &UDFExtensionCodec).expect("serialize expr");
    let round_trip =
        from_proto::parse_expr(&proto, &ctx, &UDFExtensionCodec).expect("parse expr");

    assert_eq!(format!("{:?}", &test_expr), format!("{round_trip:?}"));
    roundtrip_json_test(&proto);
}

#[test]
fn roundtrip_aggregate_udf_extension_codec() {
    let udf = AggregateUDF::from(MyAggregateUDF::new("DataFusion".to_owned()));
    let test_expr = udf.call(vec![42.lit()]);
    let ctx = SessionContext::new();
    let proto = serialize_expr(&test_expr, &UDFExtensionCodec).expect("serialize expr");
    let round_trip =
        from_proto::parse_expr(&proto, &ctx, &UDFExtensionCodec).expect("parse expr");

    assert_eq!(format!("{:?}", &test_expr), format!("{round_trip:?}"));
    roundtrip_json_test(&proto);
}

#[test]
fn roundtrip_grouping_sets() {
    let test_expr = Expr::GroupingSet(GroupingSet::GroupingSets(vec![
        vec![col("a")],
        vec![col("b")],
        vec![col("a"), col("b")],
    ]));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_rollup() {
    let test_expr = Expr::GroupingSet(GroupingSet::Rollup(vec![col("a"), col("b")]));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_cube() {
    let test_expr = Expr::GroupingSet(GroupingSet::Cube(vec![col("a"), col("b")]));

    let ctx = SessionContext::new();
    roundtrip_expr_test(test_expr, ctx);
}

#[test]
fn roundtrip_substr() {
    let ctx = SessionContext::new();

    let fun = ctx
        .state()
        .udf("substr")
        .map_err(|e| {
            internal_datafusion_err!("Unable to find expected 'substr' function: {e:?}")
        })
        .unwrap();

    // substr(string, position)
    let test_expr = Expr::ScalarFunction(ScalarFunction::new_udf(
        fun.clone(),
        vec![col("col"), lit(1_i64)],
    ));

    // substr(string, position, count)
    let test_expr_with_count = Expr::ScalarFunction(ScalarFunction::new_udf(
        fun,
        vec![col("col"), lit(1_i64), lit(1_i64)],
    ));

    roundtrip_expr_test(test_expr, ctx.clone());
    roundtrip_expr_test(test_expr_with_count, ctx);
}
#[test]
fn roundtrip_window() {
    let ctx = SessionContext::new();

    // 1. without window_frame
    let test_expr1 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::WindowUDF(rank_udwf()),
        vec![],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(true, false)])
    .window_frame(WindowFrame::new(Some(false)))
    .build()
    .unwrap();

    // 2. with default window_frame
    let test_expr2 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::WindowUDF(rank_udwf()),
        vec![],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(false, true)])
    .window_frame(WindowFrame::new(Some(false)))
    .build()
    .unwrap();

    // 3. with window_frame with row numbers
    let range_number_frame = WindowFrame::new_bounds(
        WindowFrameUnits::Range,
        WindowFrameBound::Preceding(ScalarValue::UInt64(Some(2))),
        WindowFrameBound::Following(ScalarValue::UInt64(Some(2))),
    );

    let test_expr3 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::WindowUDF(rank_udwf()),
        vec![],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(false, false)])
    .window_frame(range_number_frame)
    .build()
    .unwrap();

    // 4. test with AggregateFunction
    let row_number_frame = WindowFrame::new_bounds(
        WindowFrameUnits::Rows,
        WindowFrameBound::Preceding(ScalarValue::UInt64(Some(2))),
        WindowFrameBound::Following(ScalarValue::UInt64(Some(2))),
    );

    let test_expr4 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::AggregateUDF(max_udaf()),
        vec![col("col1")],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(true, true)])
    .window_frame(row_number_frame.clone())
    .build()
    .unwrap();

    // 5. test with AggregateUDF
    #[derive(Debug)]
    struct DummyAggr {}

    impl Accumulator for DummyAggr {
        fn state(&mut self) -> Result<Vec<ScalarValue>> {
            Ok(vec![])
        }

        fn update_batch(&mut self, _values: &[ArrayRef]) -> Result<()> {
            Ok(())
        }

        fn merge_batch(&mut self, _states: &[ArrayRef]) -> Result<()> {
            Ok(())
        }

        fn evaluate(&mut self) -> Result<ScalarValue> {
            Ok(ScalarValue::Float64(None))
        }

        fn size(&self) -> usize {
            size_of_val(self)
        }
    }

    let dummy_agg = create_udaf(
        // the name; used to represent it in plan descriptions and in the registry, to use in SQL.
        "dummy_agg",
        // the input type; DataFusion guarantees that the first entry of `values` in `update` has this type.
        vec![DataType::Float64],
        // the return type; DataFusion expects this to match the type returned by `evaluate`.
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        // This is the accumulator factory; DataFusion uses it to create new accumulators.
        Arc::new(|_| Ok(Box::new(DummyAggr {}))),
        // This is the description of the state. `state()` must match the types here.
        Arc::new(vec![DataType::Float64, DataType::UInt32]),
    );

    let test_expr5 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::AggregateUDF(Arc::new(dummy_agg.clone())),
        vec![col("col1")],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(true, true)])
    .window_frame(row_number_frame.clone())
    .build()
    .unwrap();
    ctx.register_udaf(dummy_agg);

    // 6. test with WindowUDF
    #[derive(Clone, Debug)]
    struct DummyWindow {}

    impl PartitionEvaluator for DummyWindow {
        fn uses_window_frame(&self) -> bool {
            true
        }

        fn evaluate(
            &mut self,
            _values: &[ArrayRef],
            _range: &std::ops::Range<usize>,
        ) -> Result<ScalarValue> {
            Ok(ScalarValue::Float64(None))
        }
    }

    #[derive(Debug, Clone)]
    struct SimpleWindowUDF {
        signature: Signature,
    }

    impl SimpleWindowUDF {
        fn new() -> Self {
            let signature =
                Signature::exact(vec![DataType::Float64], Volatility::Immutable);
            Self { signature }
        }
    }

    impl WindowUDFImpl for SimpleWindowUDF {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn name(&self) -> &str {
            "dummy_udwf"
        }

        fn signature(&self) -> &Signature {
            &self.signature
        }

        fn partition_evaluator(
            &self,
            _partition_evaluator_args: PartitionEvaluatorArgs,
        ) -> Result<Box<dyn PartitionEvaluator>> {
            make_partition_evaluator()
        }

        fn field(&self, field_args: WindowUDFFieldArgs) -> Result<FieldRef> {
            if let Some(return_field) = field_args.get_input_field(0) {
                Ok(return_field
                    .as_ref()
                    .clone()
                    .with_name(field_args.name())
                    .into())
            } else {
                plan_err!(
                    "dummy_udwf expects 1 argument, got {}: {:?}",
                    field_args.input_fields().len(),
                    field_args.input_fields()
                )
            }
        }
    }

    fn make_partition_evaluator() -> Result<Box<dyn PartitionEvaluator>> {
        Ok(Box::new(DummyWindow {}))
    }

    let dummy_window_udf = WindowUDF::from(SimpleWindowUDF::new());

    let test_expr6 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::WindowUDF(Arc::new(dummy_window_udf.clone())),
        vec![col("col1")],
    ))
    .partition_by(vec![col("col1")])
    .order_by(vec![col("col2").sort(true, true)])
    .window_frame(row_number_frame.clone())
    .build()
    .unwrap();

    let text_expr7 = Expr::from(expr::WindowFunction::new(
        WindowFunctionDefinition::AggregateUDF(avg_udaf()),
        vec![col("col1")],
    ))
    .window_frame(row_number_frame)
    .build()
    .unwrap();

    ctx.register_udwf(dummy_window_udf);

    roundtrip_expr_test(test_expr1, ctx.clone());
    roundtrip_expr_test(test_expr2, ctx.clone());
    roundtrip_expr_test(test_expr3, ctx.clone());
    roundtrip_expr_test(test_expr4, ctx.clone());
    roundtrip_expr_test(test_expr5, ctx.clone());
    roundtrip_expr_test(test_expr6, ctx.clone());
    roundtrip_expr_test(text_expr7, ctx);
}

#[tokio::test]
async fn roundtrip_recursive_query() {
    let query = "WITH RECURSIVE cte AS (
        SELECT 1 as n
        UNION ALL
        SELECT n + 1 FROM cte WHERE n < 5
        )
        SELECT * FROM cte;";

    let ctx = SessionContext::new();
    let dataframe = ctx.sql(query).await.unwrap();
    let plan = dataframe.logical_plan().clone();
    let output = dataframe.collect().await.unwrap();
    let bytes = logical_plan_to_bytes(&plan).unwrap();

    let ctx = SessionContext::new();
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx).unwrap();
    assert_eq!(format!("{plan:?}"), format!("{logical_round_trip:?}"));
    let dataframe = ctx.execute_logical_plan(logical_round_trip).await.unwrap();
    let output_round_trip = dataframe.collect().await.unwrap();

    assert_eq!(
        format!("{}", pretty_format_batches(&output).unwrap()),
        format!("{}", pretty_format_batches(&output_round_trip).unwrap())
    );
}

#[tokio::test]
async fn roundtrip_union_query() -> Result<()> {
    let query = "SELECT a FROM t1
        UNION (SELECT a from t1 UNION SELECT a from t2)";

    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    ctx.register_csv("t2", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    let dataframe = ctx.sql(query).await?;
    let plan = dataframe.into_optimized_plan()?;

    let bytes = logical_plan_to_bytes(&plan)?;

    let ctx = SessionContext::new();
    ctx.register_csv("t1", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    ctx.register_csv("t2", "tests/testdata/test.csv", CsvReadOptions::default())
        .await?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    // proto deserialization only supports 2-way union, hence this plan has nested unions
    // apply the flatten unions optimizer rule to be able to compare
    let optimizer = Optimizer::with_rules(vec![Arc::new(EliminateNestedUnion::new())]);
    let unnested = optimizer.optimize(logical_round_trip, &(ctx.state()), |_x, _y| {})?;
    assert_eq!(
        format!("{}", plan.display_indent_schema()),
        format!("{}", unnested.display_indent_schema()),
    );
    Ok(())
}

#[tokio::test]
async fn roundtrip_custom_listing_tables_schema() -> Result<()> {
    let ctx = SessionContext::new();
    // Make sure during round-trip, constraint information is preserved
    let file_format = JsonFormat::default();
    let table_partition_cols = vec![("part".to_owned(), DataType::Int64)];
    let data = "../core/tests/data/partitioned_table_json";
    let listing_table_url = ListingTableUrl::parse(data)?;
    let listing_options = ListingOptions::new(Arc::new(file_format))
        .with_table_partition_cols(table_partition_cols);

    let config = ListingTableConfig::new(listing_table_url)
        .with_listing_options(listing_options)
        .infer_schema(&ctx.state())
        .await?;

    ctx.register_table("hive_style", Arc::new(ListingTable::try_new(config)?))?;

    let plan = ctx
        .sql("SELECT part, value FROM hive_style LIMIT 1")
        .await?
        .logical_plan()
        .clone();

    let bytes = logical_plan_to_bytes(&plan)?;
    let new_plan = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(plan, new_plan);
    Ok(())
}

#[tokio::test]
async fn roundtrip_arrow_scan() -> Result<()> {
    let ctx = SessionContext::new();
    let plan = ctx
        .read_arrow("tests/testdata/test.arrow", ArrowReadOptions::default())
        .await?
        .into_optimized_plan()?;
    let bytes = logical_plan_to_bytes(&plan)?;
    let logical_round_trip = logical_plan_from_bytes(&bytes, &ctx)?;
    assert_eq!(format!("{plan:?}"), format!("{logical_round_trip:?}"));
    Ok(())
}
