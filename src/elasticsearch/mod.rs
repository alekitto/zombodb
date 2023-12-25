#![allow(dead_code)]

pub(crate) mod aggregates;
mod aliases;
pub(crate) mod analyze;
mod bulk;
mod cat;
mod count;
mod create_index;
mod delete_index;
mod expunge_deletes;
mod get_document;
mod get_mapping;
mod get_settings;
mod profile_query;
mod put_mapping;
mod refresh_index;
mod suggest_term;
mod update_settings;

pub mod aggregate_search;
pub mod search;

use crate::access_method::options::ZDBIndexOptions;
use crate::elasticsearch::aggregate_search::ElasticsearchAggregateSearchRequest;
use crate::elasticsearch::aliases::ElasticsearchAliasRequest;
use crate::elasticsearch::analyze::ElasticsearchAnalyzerRequest;
use crate::elasticsearch::cat::ElasticsearchCatRequest;
use crate::elasticsearch::count::ElasticsearchCountRequest;
use crate::elasticsearch::delete_index::ElasticsearchDeleteIndexRequest;
use crate::elasticsearch::expunge_deletes::ElasticsearchExpungeDeletesRequest;
use crate::elasticsearch::get_document::ElasticsearchGetDocumentRequest;
use crate::elasticsearch::get_mapping::ElasticsearchGetMappingRequest;
use crate::elasticsearch::get_settings::ElasticsearchGetSettingsRequest;
use crate::elasticsearch::pg_catalog::ArbitraryRequestType;
use crate::elasticsearch::profile_query::ElasticsearchProfileQueryRequest;
use crate::elasticsearch::put_mapping::ElasticsearchPutMappingRequest;
use crate::elasticsearch::refresh_index::ElasticsearchRefreshIndexRequest;
use crate::elasticsearch::search::ElasticsearchSearchRequest;
use crate::elasticsearch::suggest_term::ElasticsearchSuggestTermRequest;
use crate::elasticsearch::update_settings::ElasticsearchUpdateSettingsRequest;
use crate::executor_manager::get_executor_manager;
use crate::utils::is_nested_field;
use crate::zdbquery::ZDBPreparedQuery;
pub use bulk::*;
pub use create_index::*;
use lazy_static::*;
use pgrx::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::Read;

lazy_static! {
    static ref NUM_CPUS: usize = num_cpus::get();
}

#[pgrx::pg_schema]
pub mod pg_catalog {
    use pgrx::*;
    use serde::Serialize;

    #[allow(non_camel_case_types)]
    #[derive(PostgresEnum, Serialize)]
    pub enum ArbitraryRequestType {
        GET,
        POST,
        PUT,
        DELETE,
    }
}

#[derive(Clone)]
pub struct Elasticsearch {
    options: ZDBIndexOptions,
}

#[derive(Debug)]
pub struct ElasticsearchError(Option<u16>, String);

impl Display for ElasticsearchError {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            fmt,
            "{}{}",
            self.0
                .as_ref()
                .map_or(String::new(), |code| format!("HTTP {} ", code)),
            self.1
        )
    }
}

impl ElasticsearchError {
    pub fn status(&self) -> Option<u16> {
        self.0
    }

    pub fn is_404(&self) -> bool {
        self.0 == Some(404)
    }

    pub fn message(&self) -> &str {
        &self.1
    }
}

impl Elasticsearch {
    pub fn new(relation: &PgRelation) -> Self {
        Elasticsearch {
            options: ZDBIndexOptions::from_relation(relation),
        }
    }

    pub fn from_options(options: ZDBIndexOptions) -> Self {
        Elasticsearch { options }
    }

    pub fn index_relation(&self) -> PgRelation {
        self.options.index_relation()
    }

    pub fn heap_relation(&self) -> PgRelation {
        self.options.heap_relation()
    }

    pub fn is_shadow_index(&self) -> bool {
        self.options.is_shadow_index()
    }

    pub fn client() -> &'static ureq::Agent {
        lazy_static::lazy_static! {
            static ref AGENT: ureq::Agent = {
                let agent_builder = ureq::AgentBuilder::new();

                #[cfg(feature = "native_tls")] // if native_tls feature is enabled - we need to create TlsConnector and pass it to agent_builder
                {
                    let mut connector_builder = native_tls::TlsConnector::builder();
                    #[cfg(feature = "pg_test")]
                    {
                        connector_builder
                            .danger_accept_invalid_certs(true)
                            .danger_accept_invalid_hostnames(true);
                    }

                    match connector_builder.build() {
                        Ok(tls_config) => {
                            return agent_builder
                            .tls_connector(std::sync::Arc::new(tls_config))
                            .timeout_read(std::time::Duration::from_secs(3600))  // a 1hr timeout waiting on ES to return
                            .max_idle_connections_per_host(num_cpus::get())     // 1 for each CPU -- only really used during _bulk
                            .build();
                        }
                        Err(e) => {
                            // log the error
                            crate::gucs::ZDB_LOG_LEVEL.get().log(&format!(
                                "[zombodb] can't create native tls connector - {}",
                                e
                            ));
                        }
                    }

                    agent_builder
                        .timeout_read(std::time::Duration::from_secs(3600))  // a 1hr timeout waiting on ES to return
                        .max_idle_connections_per_host(num_cpus::get())     // 1 for each CPU -- only really used during _bulk
                        .build()
                }

                #[cfg(feature = "rustls")] // if rustls feature is enabled - we need to create TlsConnector and pass it to agent_builder
                {
                    use rustls::pki_types::CertificateDer;

                    fn root_certs() -> rustls::RootCertStore {
                        let mut root_cert_store = rustls::RootCertStore::empty();

                        let mut valid_count = 0;
                        let mut invalid_count = 0;
                        let certs = rustls_native_certs::load_native_certs().unwrap_or_else(|_| vec![]);
                        for cert in certs {
                            // Continue on parsing errors, as native stores often include ancient or syntactically
                            // invalid certificates, like root certificates without any X509 extensions.
                            // Inspiration: https://github.com/rustls/rustls/blob/633bf4ba9d9521a95f68766d04c22e2b01e68318/rustls/src/anchors.rs#L105-L112
                            match root_cert_store.add(CertificateDer::from(cert.0)) {
                                Ok(_) => valid_count += 1,
                                Err(_) => {
                                    invalid_count += 1;
                                }
                            }
                        }
                        if valid_count == 0 && invalid_count > 0 {
                            error!(
                                "no valid certificates loaded by rustls-native-certs. all HTTPS requests will fail."
                            );
                        }
                        root_cert_store
                    }

                    #[cfg(feature = "pg_test")]
                    let connector_builder = {
                        use rustls::client::danger::ServerCertVerifier;
                        use rustls::client::danger::ServerCertVerified;
                        use rustls::pki_types;
                        use rustls::pki_types::ServerName;
                        use rustls::client::danger::HandshakeSignatureValid;
                        use rustls::{DigitallySignedStruct, SignatureScheme};

                        #[derive(Debug)]
                        struct NoopServerVerifier;

                        impl ServerCertVerifier for NoopServerVerifier {
                            fn verify_server_cert(
                                &self,
                                _: &CertificateDer<'_>,
                                _: &[CertificateDer<'_>],
                                _: &ServerName,
                                _: &[u8],
                                _: pki_types::UnixTime,
                            ) -> Result<ServerCertVerified, rustls::Error> {
                                Ok(ServerCertVerified::assertion())
                            }

                            fn verify_tls12_signature(
                                &self,
                                _: &[u8],
                                _: &CertificateDer<'_>,
                                _: &DigitallySignedStruct,
                            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                                Ok(HandshakeSignatureValid::assertion())
                            }

                            fn verify_tls13_signature(
                                &self,
                                _: &[u8],
                                _: &CertificateDer<'_>,
                                _: &DigitallySignedStruct,
                            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                                Ok(HandshakeSignatureValid::assertion())
                            }

                            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                                vec![
                                    SignatureScheme::RSA_PKCS1_SHA1,
                                    SignatureScheme::ECDSA_SHA1_Legacy,
                                    SignatureScheme::RSA_PKCS1_SHA256,
                                    SignatureScheme::ECDSA_NISTP256_SHA256,
                                    SignatureScheme::RSA_PKCS1_SHA384,
                                    SignatureScheme::ECDSA_NISTP384_SHA384,
                                    SignatureScheme::RSA_PKCS1_SHA512,
                                    SignatureScheme::ECDSA_NISTP521_SHA512,
                                    SignatureScheme::RSA_PSS_SHA256,
                                    SignatureScheme::RSA_PSS_SHA384,
                                    SignatureScheme::RSA_PSS_SHA512,
                                    SignatureScheme::ED25519,
                                    SignatureScheme::ED448,
                                ]
                            }
                        }

                        rustls::ClientConfig::builder()
                            .dangerous()
                            .with_custom_certificate_verifier(std::sync::Arc::new(NoopServerVerifier {}))
                    };

                    #[cfg(not(feature = "pg_test"))]
                    let mut connector_builder = rustls::ClientConfig::builder();

                    agent_builder
                        .tls_config(std::sync::Arc::new(connector_builder.with_no_client_auth()))
                        .timeout_read(std::time::Duration::from_secs(3600))  // a 1hr timeout waiting on ES to return
                        .max_idle_connections_per_host(num_cpus::get())     // 1 for each CPU -- only really used during _bulk
                        .build()
                }
            };
        }

        &AGENT
    }

    pub fn arbitrary_request(
        &self,
        method: ArbitraryRequestType,
        mut endpoint: &str,
        post_data: Option<serde_json::Value>,
    ) -> Result<String, ElasticsearchError> {
        let mut url = String::new();

        if endpoint.starts_with('/') {
            url.push_str(&self.url());
            // strip the leading slash from the endpoint
            // as self.url() is required to have a trailing slash
            endpoint = &endpoint[1..];
        } else {
            url.push_str(&self.base_url());
            url.push('/');
        }

        url.push_str(endpoint);

        let request = match method {
            ArbitraryRequestType::GET => Elasticsearch::client().get(&url),
            ArbitraryRequestType::POST => Elasticsearch::client().post(&url),
            ArbitraryRequestType::PUT => Elasticsearch::client().put(&url),
            ArbitraryRequestType::DELETE => Elasticsearch::client().delete(&url),
        };

        Elasticsearch::execute_json_request(request, post_data, |body| {
            let mut response = Vec::new();
            body.read_to_end(&mut response)
                .expect("failed to read response stream");
            Ok(String::from_utf8(response).expect("arbitrary request response is not valid UTF8"))
        })
    }

    pub fn analyze_text(&self, analyzer: &str, text: &str) -> ElasticsearchAnalyzerRequest {
        ElasticsearchAnalyzerRequest::new_with_text(self, analyzer, text)
    }

    pub fn analyze_with_field(&self, field: &str, text: &str) -> ElasticsearchAnalyzerRequest {
        ElasticsearchAnalyzerRequest::new_with_field(self, field, text)
    }

    pub fn analyze_custom<'a>(
        &self,
        field: Option<default!(&'a str, NULL)>,
        text: Option<default!(&'a str, NULL)>,
        tokenizer: Option<default!(&'a str, NULL)>,
        normalizer: Option<default!(&'a str, NULL)>,
        filter: Option<default!(Array<'a, &'a str>, NULL)>,
        char_filter: Option<default!(Array<'a, &'a str>, NULL)>,
    ) -> ElasticsearchAnalyzerRequest {
        ElasticsearchAnalyzerRequest::new_custom(
            self,
            field,
            text,
            tokenizer,
            normalizer,
            filter,
            char_filter,
        )
    }

    pub fn create_index(&self, mapping: Value) -> ElasticsearchCreateIndexRequest {
        ElasticsearchCreateIndexRequest::new(self, mapping)
    }

    pub fn delete_index(&self) -> ElasticsearchDeleteIndexRequest {
        ElasticsearchDeleteIndexRequest::new(self)
    }

    pub fn refresh_index(&self) -> ElasticsearchRefreshIndexRequest {
        ElasticsearchRefreshIndexRequest::new(self)
    }

    pub fn add_alias(&self, alias_name: &str) -> ElasticsearchAliasRequest {
        ElasticsearchAliasRequest::add(self, alias_name)
    }

    pub fn remove_alias(&self, alias_name: &str) -> ElasticsearchAliasRequest {
        ElasticsearchAliasRequest::remove(self, alias_name)
    }

    pub fn expunge_deletes(&self) -> ElasticsearchExpungeDeletesRequest {
        ElasticsearchExpungeDeletesRequest::new(self)
    }

    pub fn update_settings(&self) -> ElasticsearchUpdateSettingsRequest {
        ElasticsearchUpdateSettingsRequest::new(self)
    }

    pub fn put_mapping(&self, mapping: serde_json::Value) -> ElasticsearchPutMappingRequest {
        ElasticsearchPutMappingRequest::new(self, mapping)
    }

    pub fn cat(&self, endpoint: &str) -> ElasticsearchCatRequest {
        ElasticsearchCatRequest::new(self, endpoint)
    }

    pub fn profile_query(&self, query: ZDBPreparedQuery) -> ElasticsearchProfileQueryRequest {
        ElasticsearchProfileQueryRequest::new(self, query)
    }

    pub fn start_bulk(&self) -> ElasticsearchBulkRequest {
        let concurrency = (self.options.shards() as usize)
            .min(NUM_CPUS.min(self.options.bulk_concurrency() as usize));
        ElasticsearchBulkRequest::new(
            self,
            10_000,
            concurrency,
            self.options.batch_size() as usize,
        )
    }

    pub fn open_search(&self, query: ZDBPreparedQuery) -> ElasticsearchSearchRequest {
        get_executor_manager().wait_for_completion();
        ElasticsearchSearchRequest::new(self, query)
    }

    pub fn arbitrary_aggregate<T: DeserializeOwned>(
        &self,
        field_name: Option<String>,
        need_filter: bool,
        query: ZDBPreparedQuery,
        agg_request: serde_json::Value,
    ) -> ElasticsearchAggregateSearchRequest<T> {
        if let Value::Object(agg) = agg_request {
            let mut agg_map = HashMap::new();
            agg_map.extend(agg.into_iter());
            self.aggregate_set(field_name, need_filter, query, agg_map)
        } else {
            panic!("arbitrary aggreate not in correct format")
        }
    }

    pub fn aggregate<T: DeserializeOwned>(
        &self,
        field_name: Option<String>,
        need_filter: bool,
        query: ZDBPreparedQuery,
        agg_request: serde_json::Value,
    ) -> ElasticsearchAggregateSearchRequest<T> {
        let mut aggs = HashMap::new();
        aggs.insert("the_agg".to_string(), agg_request);
        self.aggregate_set(field_name, need_filter, query, aggs)
    }

    pub fn aggregate_set<T: DeserializeOwned>(
        &self,
        field: Option<String>,
        need_filter: bool,
        query: ZDBPreparedQuery,
        aggs: HashMap<String, serde_json::Value>,
    ) -> ElasticsearchAggregateSearchRequest<T> {
        get_executor_manager().wait_for_completion();

        let (is_nested_field, nested_path, filter_query) = {
            if field.is_none() {
                // we don't have a field
                (false, None, None)
            } else {
                let field = field.as_ref().unwrap();
                // maybe it is nested, so lets go look
                let index = unsafe {
                    PgRelation::with_lock(
                        self.options.oid(),
                        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
                    )
                };

                // get the full path, which is the full field name minus the last dotted part
                let mut path = field.rsplitn(2, '.').collect::<Vec<&str>>();
                let path = path.pop().unwrap();

                if is_nested_field(&index, &path).unwrap_or(false) {
                    // is nested, so we also need to generate a filter query for it
                    if need_filter {
                        let mut value = query.query_dsl().clone();
                        let filter_query =
                            ZDBPreparedQuery::extract_nested_filter(path, Some(&mut value));
                        (true, Some(path), filter_query.cloned())
                    } else {
                        (true, Some(path), None)
                    }
                } else {
                    (false, None, None)
                }
            }
        };

        let aggs = aggs
            .into_iter()
            .map(|(k, v)| {
                if is_nested_field {
                    let nested_agg = self.make_nested_agg(&k, v, &nested_path, &filter_query);
                    (k, nested_agg)
                } else {
                    (k, v)
                }
            })
            .collect::<HashMap<String, Value>>();

        ElasticsearchAggregateSearchRequest::new(self, query, aggs)
    }

    pub fn raw_json_aggregate<T: DeserializeOwned>(
        &self,
        agg_request: serde_json::Value,
    ) -> ElasticsearchAggregateSearchRequest<T> {
        get_executor_manager().wait_for_completion();
        ElasticsearchAggregateSearchRequest::from_raw(self, agg_request)
    }

    pub fn count(&self, query: ZDBPreparedQuery) -> ElasticsearchCountRequest {
        get_executor_manager().wait_for_completion();
        ElasticsearchCountRequest::new(self, query, false)
    }

    pub fn raw_count<'a>(&self, query: ZDBPreparedQuery) -> ElasticsearchCountRequest {
        get_executor_manager().wait_for_completion();
        ElasticsearchCountRequest::new(self, query, true)
    }

    pub fn suggest_terms(
        &self,
        query: ZDBPreparedQuery,
        fieldname: String,
        suggest: String,
    ) -> ElasticsearchSuggestTermRequest {
        get_executor_manager().wait_for_completion();
        ElasticsearchSuggestTermRequest::new(self, query, fieldname, suggest)
    }

    pub fn get_document<'a, T: DeserializeOwned>(
        &self,
        id: &'a str,
        realtime: bool,
    ) -> ElasticsearchGetDocumentRequest<'a, T> {
        ElasticsearchGetDocumentRequest::<T>::new(self, id, realtime)
    }

    pub fn get_mapping(&self) -> ElasticsearchGetMappingRequest {
        ElasticsearchGetMappingRequest::new(self)
    }

    pub fn get_settings(&self) -> ElasticsearchGetSettingsRequest {
        ElasticsearchGetSettingsRequest::new(self)
    }

    pub fn url(&self) -> String {
        self.options.url()
    }

    pub fn base_url(&self) -> String {
        format!("{}{}", self.options.url(), self.options.index_name())
    }

    pub fn alias_url(&self) -> String {
        format!("{}{}", self.options.url(), self.options.alias())
    }

    pub fn index_name(&self) -> &str {
        self.options.index_name()
    }

    pub fn alias_name(&self) -> &str {
        self.options.alias()
    }

    pub fn type_name(&self) -> String {
        self.options.type_name()
    }

    pub fn execute_request<F, R, Reader: std::io::Read>(
        request: ureq::Request,
        post_data: Reader,
        response_parser: F,
    ) -> std::result::Result<R, ElasticsearchError>
    where
        F: FnOnce(&mut (dyn std::io::Read + Send)) -> std::result::Result<R, ElasticsearchError>,
    {
        Elasticsearch::handle_response(response_parser, request.send(post_data))
    }

    pub fn execute_json_request<F, R>(
        request: ureq::Request,
        post_data: Option<serde_json::Value>,
        response_parser: F,
    ) -> std::result::Result<R, ElasticsearchError>
    where
        F: FnOnce(&mut (dyn std::io::Read + Send)) -> std::result::Result<R, ElasticsearchError>,
    {
        let response = if post_data.is_some() {
            request.send_json(post_data.unwrap())
        } else {
            request.call()
        };

        Elasticsearch::handle_response(response_parser, response)
    }

    fn handle_response<F, R>(
        response_parser: F,
        response: Result<ureq::Response, ureq::Error>,
    ) -> Result<R, ElasticsearchError>
    where
        F: FnOnce(&mut (dyn Read + Send)) -> std::result::Result<R, ElasticsearchError>,
    {
        match response {
            // the request was processed by ES, but maybe not successfully
            Ok(response) => {
                let mut reader = std::io::BufReader::new(response.into_reader());
                response_parser(&mut reader)
            }

            // it wasn't a valid HTTP response code
            Err(ureq::Error::Status(code, response)) => {
                let as_string = match response.content_type() {
                    "application/json" => {
                        let value: serde_json::Value =
                            serde_json::from_reader(response.into_reader()).unwrap();
                        serde_json::to_string_pretty(&value).unwrap()
                    }

                    "application/cbor" => {
                        // this might look a little weird but we're using serde_cbor to decode
                        // the error response into a serde_json value so that we can convert it
                        // into a String
                        let value: serde_json::Value =
                            serde_cbor::from_reader(response.into_reader()).unwrap();
                        serde_json::to_string_pretty(&value).unwrap()
                    }

                    _ => response
                        .into_string()
                        .unwrap_or_else(|_| "no response data".into()),
                };

                // and return it back to the caller
                Err(ElasticsearchError(Some(code), as_string))
            }

            // the request didn't reach ES
            Err(e) => Err(ElasticsearchError(None, e.to_string())),
        }
    }

    pub fn make_nested_agg(
        &self,
        agg_name: &str,
        agg: serde_json::Value,
        path: &Option<&str>,
        filter_query: &Option<serde_json::Value>,
    ) -> serde_json::Value {
        match filter_query {
            Some(filtered_query) => json! {
                {
                    "nested": {
                        "path": path
                    },
                    "aggs": {
                        agg_name: {
                            "filter": filtered_query,
                            "aggs": {
                                agg_name: agg
                            }
                        }
                    }
                }
            },

            None => json! {
                {
                    "nested": {
                        "path": path
                    },
                    "aggs": {
                        agg_name: agg
                    }
                }
            },
        }
    }
}

#[pg_extern(volatile, parallel_safe)]
fn request(
    index: PgRelation,
    endpoint: &str,
    method: default!(ArbitraryRequestType, "'GET'"),
    post_data: default!(Option<JsonB>, NULL),
    null_on_error: default!(Option<bool>, false),
) -> Option<String> {
    let es = Elasticsearch::new(&index);
    match es.arbitrary_request(method, endpoint, post_data.map_or(None, |v| Some(v.0))) {
        Ok(response) => Some(response),
        Err(_) if null_on_error.unwrap_or(false) => None,
        Err(e) => panic!("{:?}", e),
    }
}
