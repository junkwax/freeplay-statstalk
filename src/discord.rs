use reqwest::Client;

pub struct Discord {
    client: Client,
    webhook_url: Option<String>,
}

impl Discord {
    pub fn new(webhook_url: Option<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
            webhook_url,
        }
    }

    pub fn notify_match(
        &self,
        winner_name: &str,
        loser_name: &str,
        winner_score: u16,
        loser_score: u16,
        winner_rating: f64,
        loser_rating: f64,
    ) {
        let Some(url) = &self.webhook_url else { return };
        if url.is_empty() { return; }

        let msg = format!(
            ":trophy: **{winner_name}** ({winner_score}) beats **{loser_name}** ({loser_score})\n\
            {winner_name}: **{winner_rating:.0}** rating\n\
            {loser_name}: **{loser_rating:.0}** rating",
        );

        let url = url.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.post(&url)
                .json(&serde_json::json!({ "content": msg }))
                .send()
                .await;
        });
    }
}
