use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct MozData {
    pub url_part: String,
    pub query_subdirs: bool,
    pub filter: Option<String>,
    pub data: HashSet<String>,
    pub base_url: String,
}

impl MozData {
    pub fn new(url_part: &str, filter: Option<&str>, query_subdirs: bool) -> Self {
        Self {
            url_part: url_part.to_string(),
            query_subdirs,
            filter: filter.map(str::to_string),
            data: HashSet::new(),
            base_url: "https://ftp.mozilla.org/pub".to_string(),
        }
    }

    pub async fn fetch_upstream_and_compare(&mut self) -> anyhow::Result<HashSet<String>> {
        let answer = self.query_url().await?;
        // Ignore the first iteration, where we haven't had any data yet
        let res = if self.data.is_empty() {
            HashSet::new()
        } else {
            answer.difference(&self.data).map(String::clone).collect()
        };
        self.data = answer;
        Ok(res)
    }

    async fn query_subdir(
        base_url: String,
        url_part: String,
        cand: String,
    ) -> anyhow::Result<HashSet<String>> {
        let html = reqwest::get(&format!("{}/{}/{}/", base_url, url_part, cand))
            .await?
            .text()
            .await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse("a").unwrap();
        let candidates = document
            .select(&selector)
            .map(|x| x.inner_html())
            .filter(|x| x != "..")
            .map(|x| format!("{}/{}", cand, x))
            .collect();
        Ok(candidates)
    }

    async fn query_url(&self) -> anyhow::Result<HashSet<String>> {
        let url = format!("{}/{}/", self.base_url, self.url_part);
        let html = reqwest::get(&url).await?.text().await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse("a").unwrap();
        let candidates: HashSet<_> = document
            .select(&selector)
            .map(|x| x.inner_html().trim_end_matches('/').to_string())
            .filter(|x| x != "..")
            .filter(|x| {
                if let Some(filt) = &self.filter {
                    x.contains(filt)
                } else {
                    true
                }
            })
            .collect();

        let outputs = if self.query_subdirs {
            let mut tasks = Vec::with_capacity(candidates.len());
            for cand in candidates {
                tasks.push(tokio::spawn(Self::query_subdir(
                    self.base_url.clone(),
                    self.url_part.clone(),
                    cand.clone(),
                )));
            }

            let mut outputs = HashSet::new();
            for task in tasks {
                let res = task.await?;
                outputs = outputs.union(&res?).map(String::clone).collect();
            }
            outputs
        } else {
            candidates
        };

        Ok(outputs)
    }
}
