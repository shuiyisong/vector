use std::collections::HashMap;

use crate::codecs::{Encoder, Transformer};
use crate::event::{Event, EventFinalizers, Finalizable};
use crate::http::{Auth, HttpClient, HttpError};
use crate::sinks::prelude::*;
use crate::sinks::prelude::{
    Compression, EncodeResult, Partitioner, RequestBuilder, RequestMetadata,
    RequestMetadataBuilder, RetryAction, RetryLogic,
};
use crate::sinks::{HTTPRequestBuilderSnafu, HealthcheckError};
use crate::Error;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use http::{Request, StatusCode};
use hyper::Body;
use snafu::ResultExt;

use vector_lib::codecs::encoding::Framer;

use crate::sinks::util::http::{HttpRequest, HttpResponse, HttpServiceRequestBuilder};

/// Partition key for GreptimeDB logs sink.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub(super) struct PartitionKey {
    pub dbname: String,
    pub table: String,
    pub pipeline_name: String,
    pub pipeline_version: Option<String>,
}

/// KeyPartitioner that partitions events by (dbname, table, pipeline_name, pipeline_version) pair.
pub(super) struct KeyPartitioner {
    dbname: Template,
    table: Template,
    pipeline_name: Template,
    pipeline_version: Option<Template>,
}

impl KeyPartitioner {
    pub const fn new(
        db: Template,
        table: Template,
        pipeline_name: Template,
        pipeline_version: Option<Template>,
    ) -> Self {
        Self {
            dbname: db,
            table,
            pipeline_name,
            pipeline_version,
        }
    }

    fn render(template: &Template, item: &Event, field: &'static str) -> Option<String> {
        template
            .render_string(item)
            .map_err(|error| {
                emit!(TemplateRenderingError {
                    error,
                    field: Some(field),
                    drop_event: true,
                });
            })
            .ok()
    }
}

impl Partitioner for KeyPartitioner {
    type Item = Event;
    type Key = Option<PartitionKey>;

    fn partition(&self, item: &Self::Item) -> Self::Key {
        let dbname = Self::render(&self.dbname, item, "dbname_key")?;
        let table = Self::render(&self.table, item, "table_key")?;
        let pipeline_name = Self::render(&self.pipeline_name, item, "pipeline_name")?;
        let pipeline_version = self
            .pipeline_version
            .as_ref()
            .and_then(|template| Self::render(template, item, "pipeline_version"));
        Some(PartitionKey {
            dbname,
            table,
            pipeline_name,
            pipeline_version,
        })
    }
}

/// GreptimeDB logs HTTP request builder.
#[derive(Debug, Clone)]
pub(super) struct GreptimeDBLogsHttpRequestBuilder {
    pub(super) endpoint: String,
    pub(super) auth: Option<Auth>,
    pub(super) encoder: (Transformer, Encoder<Framer>),
    pub(super) compression: Compression,
    pub(super) extra_params: Option<HashMap<String, String>>,
}

/*
{
    "bytes": 2087,
    "http_version": "HTTP/1.1",
    "ip": "225.144.116.48",
    "method": "PUT",
    "path": "/user/booperbot124",
    "status": 404,
    "timestamp": "2024-08-08T03:32:20Z",
    "user": "ahmadajmi"
}
*/
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LogItem {
    bytes: u64,
    http_version: String,
    ip: String,
    method: String,
    path: String,
    status: u16,
    timestamp: DateTime<Utc>,
    user: String,
}

impl LogItem {
    fn to_value(&self) -> String {
        format!(
            "({}, '{}', '{}', '{}', '{}', {}, {}, '{}')",
            self.bytes,
            self.http_version,
            self.ip,
            self.method,
            self.path,
            self.status,
            self.timestamp.timestamp_millis(),
            self.user
        )
    }
}

impl HttpServiceRequestBuilder<PartitionKey> for GreptimeDBLogsHttpRequestBuilder {
    fn build(&self, mut request: HttpRequest<PartitionKey>) -> Result<Request<Bytes>, Error> {
        let metadata = request.get_additional_metadata();
        let table = metadata.table.clone();
        let db = metadata.dbname.clone();

        // prepare url
        let endpoint = format!("{}/v1/sql", self.endpoint.as_str());
        let mut url = url::Url::parse(&endpoint).unwrap();
        let mut url_builder = url.query_pairs_mut();
        url_builder.append_pair("db", &db);

        if let Some(extra_params) = self.extra_params.as_ref() {
            for (key, value) in extra_params.iter() {
                url_builder.append_pair(key, value);
            }
        }

        // prepare body
        let payload = request.take_payload();
        let p = String::from_utf8_lossy(&payload).to_owned().to_string();

        // CREATE TABLE IF NOT EXISTS `ngx_access_log` (
        //     `bytes` Int64 NULL,
        //     `http_version` STRING NULL,
        //     `ip` STRING NULL,
        //     `method` STRING NULL,
        //     `path` STRING NULL,
        //     `status` SMALLINT UNSIGNED NULL,
        //     `user` STRING NULL,
        //     `timestamp` TIMESTAMP(3) NOT NULL,
        //     TIME INDEX (`timestamp`)
        //   )
        //   ENGINE=mito
        //   WITH(
        //     append_mode = 'true'
        //   );

        let mut sql = String::new();
        sql.push_str("insert into ");
        sql.push_str(&table);
        sql.push_str("(bytes, http_version, ip, method, path, status, timestamp, user) values ");
        p.split("\n").for_each(|line| {
            let item: LogItem = serde_json::from_str(line).unwrap();
            let value = item.to_value();
            sql.push_str(&value);
            sql.push_str(",");
        });
        sql.pop();
        sql.push_str(";");

        url_builder.append_pair("sql", &sql);

        let url = url_builder.finish().to_string();

        let mut builder =
            Request::post(&url).header(CONTENT_TYPE, "application/x-www-form-urlencoded");

        if let Some(ce) = self.compression.content_encoding() {
            builder = builder.header(CONTENT_ENCODING, ce);
        }

        if let Some(auth) = self.auth.clone() {
            builder = auth.apply_builder(builder);
        }

        builder
            .body(payload)
            .context(HTTPRequestBuilderSnafu)
            .map_err(Into::into)
    }
}

impl RequestBuilder<(PartitionKey, Vec<Event>)> for GreptimeDBLogsHttpRequestBuilder {
    type Metadata = (PartitionKey, EventFinalizers);
    type Events = Vec<Event>;
    type Encoder = (Transformer, Encoder<Framer>);
    type Payload = Bytes;
    type Request = HttpRequest<PartitionKey>;
    type Error = std::io::Error;

    fn compression(&self) -> Compression {
        self.compression
    }

    fn encoder(&self) -> &Self::Encoder {
        &self.encoder
    }

    fn split_input(
        &self,
        input: (PartitionKey, Vec<Event>),
    ) -> (Self::Metadata, RequestMetadataBuilder, Self::Events) {
        let (key, mut events) = input;

        let finalizers = events.take_finalizers();
        let builder = RequestMetadataBuilder::from_events(&events);
        ((key, finalizers), builder, events)
    }

    fn build_request(
        &self,
        metadata: Self::Metadata,
        request_metadata: RequestMetadata,
        payload: EncodeResult<Self::Payload>,
    ) -> Self::Request {
        let (key, finalizers) = metadata;
        HttpRequest::new(
            payload.into_payload(),
            finalizers,
            request_metadata,
            PartitionKey {
                dbname: key.dbname,
                table: key.table,
                pipeline_name: key.pipeline_name,
                pipeline_version: key.pipeline_version,
            },
        )
    }
}

pub(super) async fn http_healthcheck(
    client: HttpClient,
    endpoint: String,
    auth: Option<Auth>,
) -> crate::Result<()> {
    let uri = format!("{endpoint}/health");
    let mut request = Request::get(uri).body(Body::empty()).unwrap();

    if let Some(auth) = auth {
        auth.apply(&mut request);
    }

    let response = client.send(request).await?;

    match response.status() {
        StatusCode::OK => Ok(()),
        status => Err(HealthcheckError::UnexpectedStatus { status }.into()),
    }
}

/// GreptimeDB HTTP retry logic.
#[derive(Clone, Default)]
pub(super) struct GreptimeDBHttpRetryLogic;

impl RetryLogic for GreptimeDBHttpRetryLogic {
    type Error = HttpError;
    type Response = HttpResponse;

    fn is_retriable_error(&self, _error: &Self::Error) -> bool {
        true
    }

    fn should_retry_response(&self, response: &Self::Response) -> RetryAction {
        let status = response.http_response.status();
        match status {
            StatusCode::INTERNAL_SERVER_ERROR => {
                let body = response.http_response.body();

                // Currently, ClickHouse returns 500's incorrect data and type mismatch errors.
                // This attempts to check if the body starts with `Code: {code_num}` and to not
                // retry those errors.
                //
                // Reference: https://github.com/vectordotdev/vector/pull/693#issuecomment-517332654
                // Error code definitions: https://github.com/ClickHouse/ClickHouse/blob/master/dbms/src/Common/ErrorCodes.cpp
                //
                // Fix already merged: https://github.com/ClickHouse/ClickHouse/pull/6271
                if body.starts_with(b"Code: 117") {
                    RetryAction::DontRetry("incorrect data".into())
                } else if body.starts_with(b"Code: 53") {
                    RetryAction::DontRetry("type mismatch".into())
                } else {
                    RetryAction::Retry(String::from_utf8_lossy(body).to_string().into())
                }
            }
            StatusCode::TOO_MANY_REQUESTS => RetryAction::Retry("too many requests".into()),
            StatusCode::NOT_IMPLEMENTED => {
                RetryAction::DontRetry("endpoint not implemented".into())
            }
            _ if status.is_server_error() => RetryAction::Retry(
                format!(
                    "{}: {}",
                    status,
                    String::from_utf8_lossy(response.http_response.body())
                )
                .into(),
            ),
            _ if status.is_success() => RetryAction::Successful,
            _ => RetryAction::DontRetry(format!("response status: {}", status).into()),
        }
    }
}
