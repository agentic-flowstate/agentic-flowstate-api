use std::sync::Arc;
use aws_config::BehaviorVersion;
use aws_sdk_dynamodb::{Client, Config};
use once_cell::sync::OnceCell;
use anyhow::Result;

static DYNAMODB_CLIENT: OnceCell<Arc<Client>> = OnceCell::new();

#[derive(Clone)]
pub struct DynamoDbPool {
    client: Arc<Client>,
    table_name: String,
}

impl DynamoDbPool {
    pub async fn new() -> Result<Self> {
        // Initialize the client only once and reuse it
        let client = if let Some(client) = DYNAMODB_CLIENT.get() {
            Arc::clone(client)
        } else {
            // Use the telemetryops-shared profile
            std::env::set_var("AWS_PROFILE", "telemetryops-shared");

            let config = aws_config::defaults(BehaviorVersion::latest())
                .profile_name("telemetryops-shared")
                .load()
                .await;

            let dynamodb_config = Config::new(&config);
            let client = Arc::new(Client::from_conf(dynamodb_config));

            DYNAMODB_CLIENT.set(Arc::clone(&client))
                .map_err(|_| anyhow::anyhow!("Failed to set DynamoDB client"))?;

            client
        };

        Ok(Self {
            client,
            table_name: "tickets".to_string(),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}