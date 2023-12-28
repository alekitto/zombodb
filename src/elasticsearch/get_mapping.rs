use crate::elasticsearch::{Elasticsearch, ElasticsearchError};

pub struct ElasticsearchGetMappingRequest(Elasticsearch);

impl ElasticsearchGetMappingRequest {
    pub fn new(elasticsearch: &Elasticsearch) -> Self {
        ElasticsearchGetMappingRequest(elasticsearch.clone())
    }

    pub fn execute(self) -> Result<serde_json::Value, ElasticsearchError> {
        Elasticsearch::execute_json_request(
            self.0
                .client()
                .get(&format!("{}/_mapping", self.0.base_url())),
            None,
            |body| Ok(serde_json::from_reader(body).expect("failed to read json response")),
        )
    }
}
