use serde::{Deserialize, Serialize};
use surf::http::Method;

#[derive(Deserialize)]
pub struct Response {
    pub hits: Hits,
}

impl Response {
    pub fn title(&self) -> Option<&str> {
        self.hits
            .hits
            .iter()
            .flat_map(|hit| hit.metadata.titles.iter())
            .next()
            .map(|title| &title.title[..])
    }
}

#[derive(Deserialize)]
pub struct Hits {
    pub hits: Vec<NestedHit>,
}

#[derive(Deserialize)]
pub struct NestedHit {
    pub metadata: Metadata,
}

#[derive(Deserialize)]
pub struct Metadata {
    pub titles: Vec<Title>,
}

impl Metadata {
    pub fn title(&self) -> Option<&str> {
        self.titles.get(0).map(|t| &t.title[..])
    }
}

#[derive(Deserialize)]
pub struct Title {
    pub title: String,
}

#[derive(Serialize)]
struct Query {
    pub q: String,
}

pub async fn get(input: String) -> Result<Response, surf::Error> {
    let mut response = surf::RequestBuilder::new(
        Method::Get,
        "https://inspirehep.net/api/literature".try_into().unwrap(),
    )
    .query(&Query { q: input })?
    .await?;

    Ok(response.body_json::<Response>().await?)
}
