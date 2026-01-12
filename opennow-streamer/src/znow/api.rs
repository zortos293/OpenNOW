//! ZNow API Client
//!
//! Fetches app catalog from the ZNow relay server.

use crate::app::types::ZNowApp;
use reqwest::Client;
use tracing::{info, error};

const DEFAULT_RELAY_URL: &str = "https://znow.zortos.me";

pub struct ZNowApiClient {
    client: Client,
    base_url: String,
}

impl ZNowApiClient {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_RELAY_URL)
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.to_string(),
        }
    }

    /// Fetch all available apps
    pub async fn fetch_apps(&self) -> Result<Vec<ZNowApp>, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/apps", self.base_url);
        info!("Fetching ZNow apps from {}", url);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Failed to fetch apps: {} - {}", status, body);
            return Err(format!("API error: {}", status).into());
        }

        let apps: Vec<ZNowApp> = response.json().await?;
        info!("Fetched {} ZNow apps", apps.len());
        Ok(apps)
    }

    /// Fetch apps by category
    pub async fn fetch_apps_by_category(
        &self,
        category: &str,
    ) -> Result<Vec<ZNowApp>, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/apps/{}", self.base_url, category);
        info!("Fetching ZNow apps for category: {}", category);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(format!("API error: {}", status).into());
        }

        let apps: Vec<ZNowApp> = response.json().await?;
        Ok(apps)
    }
}

impl Default for ZNowApiClient {
    fn default() -> Self {
        Self::new()
    }
}
