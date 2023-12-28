use crate::elasticsearch::{Elasticsearch, ElasticsearchError};
use serde_json::json;

enum AliasCommand {
    Add(String),
    Remove(String),
}

pub struct ElasticsearchAliasRequest {
    elasticsearch: Elasticsearch,
    command: AliasCommand,
}

impl ElasticsearchAliasRequest {
    pub fn add(elasticsearch: &Elasticsearch, alias_name: &str) -> Self {
        ElasticsearchAliasRequest {
            elasticsearch: elasticsearch.clone(),
            command: AliasCommand::Add(alias_name.to_owned()),
        }
    }

    pub fn remove(elasticsearch: &Elasticsearch, alias_name: &str) -> Self {
        ElasticsearchAliasRequest {
            elasticsearch: elasticsearch.clone(),
            command: AliasCommand::Remove(alias_name.to_owned()),
        }
    }

    pub fn execute(self) -> std::result::Result<(), ElasticsearchError> {
        let json_body = match &self.command {
            AliasCommand::Add(alias_name) => {
                json! {
                    {
                       "actions": [
                            {"add": { "index": self.elasticsearch.index_name(), "alias": alias_name } }
                        ]
                    }
                }
            }

            AliasCommand::Remove(alias_name) => {
                json! {
                    {
                       "actions": [
                            {"remove": { "index": self.elasticsearch.index_name(), "alias": alias_name } }
                        ]
                    }
                }
            }
        };

        Elasticsearch::execute_json_request(
            self.elasticsearch
                .client()
                .post(&format!("{}_aliases", self.elasticsearch.url())),
            Some(json_body),
            |_| Ok(()),
        )
    }
}
