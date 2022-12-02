use serde::{Deserialize, Serialize};
use surf::http::Method;

#[derive(Deserialize, Debug)]
pub struct InspiresSearchResult {
    pub hits: Hits,
}

#[derive(Deserialize, Debug)]
pub struct Hits {
    pub hits: Vec<NestedHit>,
}

#[derive(Deserialize, Debug)]
pub struct NestedHit {
    pub metadata: Metadata,
}

#[derive(Deserialize, Debug)]
pub struct Metadata {
    pub titles: Vec<Title>,
    #[serde(default)]
    pub arxiv_eprints: Vec<ArxivEprint>,
    #[serde(default)]
    pub authors: Vec<Author>,
}

#[derive(Deserialize, Debug)]
pub struct Author {
    pub last_name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ArxivEprint {
    pub value: String,
}

impl Metadata {
    pub fn title(&self) -> Option<&str> {
        self.titles.get(0).map(|t| &t.title[..])
    }

    pub fn authors(&self) -> String {
        let mut authors = String::new();

        for author in &self.authors {
            let Some(last_name) = &author.last_name else {
                continue;
            };

            if !authors.is_empty() {
                authors.push_str(", ");
            }

            authors.push_str(last_name);
        }

        authors
    }

    pub fn eprint(&self) -> Option<&str> {
        self.arxiv_eprints.get(0).map(|e| &e.value[..])
    }

    pub fn eprints(&self) -> impl ExactSizeIterator<Item = &str> {
        self.arxiv_eprints.iter().map(|entry| entry.value.as_ref())
    }
}

#[derive(Deserialize, Debug)]
pub struct Title {
    pub title: String,
}

#[derive(Serialize)]
struct InspiresQuery {
    q: String,
    sort: &'static str,
    size: u32,
    fields: &'static str,
}

#[derive(Serialize)]
struct ArxivQuery {
    id_list: String,
}

pub async fn search_inspires(input: String) -> Result<InspiresSearchResult, surf::Error> {
    let mut response = surf::RequestBuilder::new(
        Method::Get,
        "https://inspirehep.net/api/literature".try_into().unwrap(),
    )
    .query(&InspiresQuery {
        q: input,
        sort: "mostrecent",
        size: 50,
        fields: "titles,arxiv_eprints,authors",
    })?
    .await?;

    Ok(response.body_json::<InspiresSearchResult>().await?)
}

#[derive(Deserialize, Debug)]
pub struct ArxivSearchResult {
    pub entry: Vec<ArxivEntry>,
}

#[derive(Deserialize, Debug)]
pub struct ArxivEntry {
    pub id: String,
    pub link: Vec<Link>,
}

#[derive(Deserialize, Debug)]
pub struct Link {
    pub title: Option<String>,
    pub href: String,
}

pub async fn get_preprint(id: String) -> surf::Result<ArxivSearchResult> {
    let mut response = surf::RequestBuilder::new(
        Method::Get,
        "http://export.arxiv.org/api/query".try_into().unwrap(),
    )
    .query(&ArxivQuery { id_list: id })?
    .await?;

    let body = response.body_string().await?;
    Ok(quick_xml::de::from_str(&body)?)
}
