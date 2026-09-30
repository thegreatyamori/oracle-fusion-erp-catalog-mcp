use crate::db::{CatalogTable, Database};
use anyhow::{anyhow, Context, Result};
use indicatif::ProgressBar;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinSet;
use url::Url;

const FETCH_CONCURRENCY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleModule {
    Financials,
    Scm,
    Hcm,
}

impl OracleModule {
    fn path(self) -> &'static str {
        match self {
            Self::Financials => "financials",
            Self::Scm => "supply-chain-and-manufacturing",
            Self::Hcm => "human-resources",
        }
    }

    fn guide(self) -> &'static str {
        match self {
            Self::Financials => "oedmf",
            Self::Scm => "oedsc",
            Self::Hcm => "oedmh",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Financials => "FINANCIALS",
            Self::Scm => "SCM",
            Self::Hcm => "HCM",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OracleSource {
    pub module: OracleModule,
    pub release: String,
    pub index_url: Url,
}

impl OracleSource {
    pub fn help_center(module: OracleModule, release: &str) -> Result<Self> {
        let release = release.to_ascii_lowercase();
        let url = if module == OracleModule::Hcm {
            format!(
                "https://docs.oracle.com/en/cloud/saas/{}/{}/index.html",
                module.path(),
                module.guide()
            )
        } else {
            format!(
                "https://docs.oracle.com/en/cloud/saas/{}/{}/{}/index.html",
                module.path(),
                release,
                module.guide()
            )
        };
        Ok(Self {
            module,
            release,
            index_url: Url::parse(&url)?,
        })
    }
}

fn guide_prefix(source: &OracleSource) -> String {
    if source.module == OracleModule::Hcm {
        format!(
            "/en/cloud/saas/{}/{}/",
            source.module.path(),
            source.module.guide()
        )
    } else {
        format!(
            "/en/cloud/saas/{}/{}/{}/",
            source.module.path(),
            source.release,
            source.module.guide()
        )
    }
}

#[derive(Debug, Deserialize)]
struct JsonCatalog {
    #[serde(default)]
    tables: Vec<CatalogTable>,
}

pub struct OracleExtractor {
    client: Client,
}

impl OracleExtractor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(format!(
                    "oracle-fusion-erp-catalog-mcp/{}",
                    env!("CARGO_PKG_VERSION")
                ))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .pool_max_idle_per_host(FETCH_CONCURRENCY)
                .tcp_nodelay(true)
                .http1_only()
                .build()?,
        })
    }

    pub async fn extract(&self, source: &OracleSource) -> Result<Vec<CatalogTable>> {
        self.extract_with_progress(source, None).await
    }

    pub async fn extract_with_progress(
        &self,
        source: &OracleSource,
        progress: Option<&ProgressBar>,
    ) -> Result<Vec<CatalogTable>> {
        if let Some(tables) = self.extract_from_toc(source, progress).await? {
            return Ok(tables);
        }
        let response = self
            .client
            .get(source.index_url.clone())
            .send()
            .await
            .with_context(|| format!("downloading {}", source.index_url))?
            .error_for_status()?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = response.bytes().await?;
        if content_type.contains("json") || bytes.first() == Some(&b'{') {
            let document: JsonCatalog = serde_json::from_slice(&bytes)?;
            return Ok(document.tables);
        }
        if content_type.contains("xml") || bytes.starts_with(b"<?xml") {
            return parse_xml_catalog(&bytes);
        }
        self.extract_html_guide(&bytes, source, progress).await
    }

    async fn extract_html_guide(
        &self,
        initial_payload: &[u8],
        source: &OracleSource,
        progress: Option<&ProgressBar>,
    ) -> Result<Vec<CatalogTable>> {
        let selectors = GuideSelectors::new()?;
        let prefix = guide_prefix(source);
        let mut current_url = source.index_url.clone();
        let mut current_payload = initial_payload.to_vec();
        let mut seen = BTreeSet::new();
        let mut tables = Vec::new();

        for page_number in 0usize..10_000 {
            if !seen.insert(current_url.as_str().to_owned()) {
                break;
            }
            let (table, next_href) =
                parse_guide_page(&current_payload, &current_url, source, &selectors)?;
            if let Some(table) = table {
                tables.push(table);
            }
            if page_number > 0 && page_number.is_multiple_of(100) {
                let message = format!(
                    "processed {} Oracle guide pages and {} tables",
                    page_number,
                    tables.len()
                );
                if let Some(progress) = progress {
                    progress.set_message(message);
                } else {
                    eprintln!("{message}");
                }
            }

            let Some(href) = next_href else {
                break;
            };
            let next_url = current_url.join(&href)?;
            if next_url.host_str() != Some("docs.oracle.com")
                || !next_url.path().starts_with(&prefix)
            {
                break;
            }
            current_payload = self
                .client
                .get(next_url.clone())
                .send()
                .await
                .with_context(|| format!("downloading {}", next_url))?
                .error_for_status()?
                .bytes()
                .await?
                .to_vec();
            current_url = next_url;
        }
        Ok(tables)
    }

    async fn extract_from_toc(
        &self,
        source: &OracleSource,
        progress: Option<&ProgressBar>,
    ) -> Result<Option<Vec<CatalogTable>>> {
        let toc_url = source.index_url.join("toc.htm")?;
        let response = match self.client.get(toc_url.clone()).send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() || error.is_connect() => {
                return Err(anyhow!(error).context(format!("downloading {toc_url}")));
            }
            Err(_) => return Ok(None),
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let bytes = response.bytes().await?;
        let prefix = guide_prefix(source);
        let links = toc_links(&bytes, &toc_url, &prefix)?;
        if links.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.fetch_tables(links, source, progress).await?))
    }

    async fn fetch_tables(
        &self,
        links: Vec<Url>,
        source: &OracleSource,
        progress: Option<&ProgressBar>,
    ) -> Result<Vec<CatalogTable>> {
        let selectors = Arc::new(GuideSelectors::new()?);
        let total = links.len();
        let mut pending = links.into_iter().enumerate();
        let mut tasks: JoinSet<Result<(usize, Option<CatalogTable>)>> = JoinSet::new();
        let mut slots = vec![None; total];
        let mut completed = 0usize;
        let mut found = 0usize;

        let spawn = |tasks: &mut JoinSet<Result<(usize, Option<CatalogTable>)>>,
                     index: usize,
                     url: Url,
                     client: Client,
                     source: OracleSource,
                     selectors: Arc<GuideSelectors>| {
            tasks.spawn(async move {
                let bytes = download_with_retry(&client, &url).await?;
                let table = parse_guide_page(&bytes, &url, &source, &selectors)?.0;
                Ok((index, table))
            });
        };

        for _ in 0..FETCH_CONCURRENCY {
            let Some((index, url)) = pending.next() else {
                break;
            };
            spawn(
                &mut tasks,
                index,
                url,
                self.client.clone(),
                source.clone(),
                Arc::clone(&selectors),
            );
        }

        while let Some(joined) = tasks.join_next().await {
            let parsed = match joined {
                Ok(result) => result,
                Err(error) => {
                    tasks.abort_all();
                    return Err(anyhow!("page task failed: {error}"));
                }
            };
            let (index, table) = match parsed {
                Ok(parsed) => parsed,
                Err(error) => {
                    tasks.abort_all();
                    return Err(error);
                }
            };
            if table.is_some() {
                found += 1;
            }
            slots[index] = table;
            completed += 1;
            if completed.is_multiple_of(100) {
                let message = format!("fetched {completed}/{total} guide pages and {found} tables");
                if let Some(progress) = progress {
                    progress.set_message(message);
                } else {
                    eprintln!("{message}");
                }
            }
            if let Some((index, url)) = pending.next() {
                spawn(
                    &mut tasks,
                    index,
                    url,
                    self.client.clone(),
                    source.clone(),
                    Arc::clone(&selectors),
                );
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }
}

#[cfg(test)]
fn parse_table_page(
    payload: &[u8],
    page_url: &Url,
    source: &OracleSource,
) -> Result<Option<CatalogTable>> {
    let selectors = GuideSelectors::new()?;
    Ok(parse_guide_page(payload, page_url, source, &selectors)?.0)
}

fn parse_guide_page(
    payload: &[u8],
    page_url: &Url,
    source: &OracleSource,
    selectors: &GuideSelectors,
) -> Result<(Option<CatalogTable>, Option<String>)> {
    let html = std::str::from_utf8(payload).context("HTML page is not UTF-8")?;
    let document = Html::parse_document(html);
    let next_href = document
        .select(&selectors.next_link)
        .next()
        .and_then(|link| link.value().attr("href"))
        .map(str::to_owned);
    let Some(columns_table) = document.select(&selectors.columns).next() else {
        return Ok((None, next_href));
    };
    let Some(title) = document.select(&selectors.title).next() else {
        return Ok((None, next_href));
    };
    let table_name = text_content(title).to_ascii_uppercase();
    if table_name.is_empty() {
        return Ok((None, next_href));
    }

    let description = document
        .select(&selectors.description)
        .next()
        .and_then(|meta| meta.value().attr("content"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let mut columns = Vec::new();
    for row in columns_table.select(&selectors.row) {
        let cells: Vec<_> = row.select(&selectors.cell).collect();
        if cells.len() < 6 {
            continue;
        }
        let column_name = text_content(cells[0]).to_ascii_uppercase();
        if column_name.is_empty() {
            continue;
        }
        let length_text = text_content(cells[2]);
        let precision_text = text_content(cells[3]);
        let length = length_text
            .parse()
            .ok()
            .or_else(|| precision_text.parse().ok());
        columns.push(crate::db::CatalogColumn {
            column_name,
            data_type: text_content(cells[1]),
            length,
            nullable: !text_content(cells[4]).eq_ignore_ascii_case("yes"),
            description: non_empty_text(cells[5]),
        });
    }

    let references = parse_foreign_keys(&document, selectors)?;
    let indexes = parse_indexes(&document, selectors)?;
    Ok((
        Some(CatalogTable {
            module: source.module.label().to_owned(),
            table_name,
            description,
            source_url: Some(page_url.to_string()),
            object_type: Some(object_type_label(&document)),
            columns,
            references,
            indexes,
        }),
        next_href,
    ))
}

fn parse_foreign_keys(
    document: &Html,
    selectors: &GuideSelectors,
) -> Result<Vec<crate::db::CatalogReference>> {
    let Some(foreign_keys_table) = document.select(&selectors.foreign_keys).next() else {
        return Ok(Vec::new());
    };
    let mut references = Vec::new();
    for row in foreign_keys_table.select(&selectors.row) {
        let cells: Vec<_> = row.select(&selectors.cell).collect();
        if cells.len() < 3 {
            continue;
        }
        let target_table = text_content(cells[1]).to_ascii_uppercase();
        let source_column = text_content(cells[2]).to_ascii_uppercase();
        if target_table.is_empty() || source_column.is_empty() {
            continue;
        }
        references.push(crate::db::CatalogReference {
            target_table,
            source_column,
            target_column: None,
            constraint_name: None,
        });
    }
    Ok(references)
}

fn parse_indexes(
    document: &Html,
    selectors: &GuideSelectors,
) -> Result<Vec<crate::db::CatalogIndex>> {
    let Some(indexes_table) = document.select(&selectors.indexes).next() else {
        return Ok(Vec::new());
    };
    let mut indexes = BTreeMap::new();
    for row in indexes_table.select(&selectors.row) {
        let cells: Vec<_> = row.select(&selectors.cell).collect();
        if cells.len() < 4 {
            continue;
        }
        let index_name = text_content(cells[0]).to_ascii_uppercase();
        if index_name.is_empty() {
            continue;
        }
        let entry = indexes
            .entry(index_name.clone())
            .or_insert_with(|| crate::db::CatalogIndex {
                index_name,
                indexed_columns: Vec::new(),
                is_unique: false,
            });
        entry.is_unique |= text_content(cells[1]).eq_ignore_ascii_case("unique");
        for column in text_content(cells[3])
            .split(',')
            .map(str::trim)
            .filter(|column| !column.is_empty())
        {
            if !entry
                .indexed_columns
                .iter()
                .any(|existing| existing == column)
            {
                entry.indexed_columns.push(column.to_owned());
            }
        }
    }
    Ok(indexes.into_values().collect())
}

fn text_content(element: scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn non_empty_text(element: scraper::ElementRef<'_>) -> Option<String> {
    let text = text_content(element);
    (!text.is_empty()).then_some(text)
}

struct GuideSelectors {
    columns: Selector,
    title: Selector,
    description: Selector,
    row: Selector,
    cell: Selector,
    foreign_keys: Selector,
    indexes: Selector,
    next_link: Selector,
}

impl GuideSelectors {
    fn new() -> Result<Self> {
        Ok(Self {
            columns: selector(r#"table[summary="Columns"]"#)?,
            title: selector("h1 .chapterstart")?,
            description: selector(r#"meta[name="description"]"#)?,
            row: selector("tbody tr")?,
            cell: selector("td")?,
            foreign_keys: selector(r#"table[summary="Foreign Keys"]"#)?,
            indexes: selector(r#"table[summary="Indexes"]"#)?,
            next_link: selector(r#"link[rel="next"]"#)?,
        })
    }
}

fn selector(source: &str) -> Result<Selector> {
    Selector::parse(source).map_err(|error| anyhow!("invalid selector: {error}"))
}

fn object_type_label(document: &Html) -> String {
    let mut text = String::new();
    for piece in document.root_element().text() {
        text.push_str(piece);
        text.push(' ');
    }
    let marker = "Object type:";
    let Some(index) = text.find(marker) else {
        return "TABLE".to_owned();
    };
    let word = text[index + marker.len()..]
        .split_whitespace()
        .next()
        .unwrap_or("TABLE");
    let word = word.trim_matches(|character: char| !character.is_ascii_alphanumeric());
    if word.is_empty() {
        "TABLE".to_owned()
    } else {
        word.to_ascii_uppercase()
    }
}

fn toc_links(payload: &[u8], base: &Url, prefix: &str) -> Result<Vec<Url>> {
    let html = std::str::from_utf8(payload).context("TOC is not UTF-8")?;
    let document = Html::parse_document(html);
    let anchors = selector("a[href]")?;
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    for anchor in document.select(&anchors) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Ok(mut url) = base.join(href) else {
            continue;
        };
        url.set_fragment(None);
        url.set_query(None);
        if url.host_str() != Some("docs.oracle.com") || !url.path().starts_with(prefix) {
            continue;
        }
        if !url.path().ends_with(".html") {
            continue;
        }
        let file_name = url.path().rsplit('/').next().unwrap_or("");
        if file_name == "index.html" || file_name.starts_with("toc.") {
            continue;
        }
        if seen.insert(url.as_str().to_owned()) {
            links.push(url);
        }
    }
    Ok(links)
}

async fn download_with_retry(client: &Client, url: &Url) -> Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt as u32))).await;
        }
        match client.get(url.clone()).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response.bytes().await?.to_vec());
                }
                if status.as_u16() == 429 || status.is_server_error() {
                    last_error = Some(anyhow!("{status} for {url}"));
                    continue;
                }
                return Err(anyhow!("{status} for {url}"));
            }
            Err(error) => {
                last_error = Some(anyhow!(error).context(format!("downloading {url}")));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("download failed for {url}")))
}

fn parse_xml_catalog(payload: &[u8]) -> Result<Vec<CatalogTable>> {
    #[derive(Debug, Deserialize)]
    struct XmlCatalog {
        #[serde(rename = "table", default)]
        tables: Vec<CatalogTable>,
    }
    let document: XmlCatalog = quick_xml::de::from_reader(payload)?;
    Ok(document.tables)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseCode {
    year: u16,
    cycle: u8,
}

fn parse_release_code(value: &str) -> Result<ReleaseCode> {
    let normalized = value.trim().to_ascii_uppercase();
    if normalized.len() < 2 {
        return Err(anyhow!("invalid release code: {value}"));
    }
    let (year, cycle) = normalized.split_at(normalized.len() - 1);
    if !year.chars().all(|character| character.is_ascii_digit())
        || !cycle.as_bytes()[0].is_ascii_uppercase()
    {
        return Err(anyhow!("invalid release code: {value}"));
    }
    Ok(ReleaseCode {
        year: year.parse()?,
        cycle: cycle.as_bytes()[0] - b'A',
    })
}

fn is_superior_release(candidate: &str, current: &str) -> Result<bool> {
    Ok(parse_release_code(candidate)? > parse_release_code(current)?)
}

pub fn synchronize(
    db: &Database,
    release: &str,
    tables: Vec<CatalogTable>,
    activate: bool,
) -> Result<i64> {
    synchronize_with_progress(db, release, tables, activate, |_, _| {})
}

pub fn synchronize_with_progress<F>(
    db: &Database,
    release: &str,
    tables: Vec<CatalogTable>,
    activate: bool,
    on_progress: F,
) -> Result<i64>
where
    F: FnMut(usize, usize),
{
    synchronize_with_progress_internal(db, release, tables, activate, on_progress, true)
}

fn synchronize_with_progress_internal<F>(
    db: &Database,
    release: &str,
    mut tables: Vec<CatalogTable>,
    activate: bool,
    on_progress: F,
    rebuild_search: bool,
) -> Result<i64>
where
    F: FnMut(usize, usize),
{
    let release = release.trim().to_ascii_uppercase();
    parse_release_code(&release)?;
    let previous = db.active_version()?;
    let target = db.version_by_release(&release)?;
    let incoming_modules: BTreeSet<_> = tables.iter().map(|table| table.module.clone()).collect();
    if incoming_modules.is_empty() {
        return Err(anyhow!("sync produced no modules"));
    }

    let version_id = if let Some(existing) = target {
        let existing_modules = db.modules_for_version(existing.id)?;
        if incoming_modules
            .iter()
            .any(|module| existing_modules.contains(module))
        {
            return Err(anyhow!(
                "release {release} already contains one of the synchronized modules"
            ));
        }
        if let Some(previous) = &previous {
            if !is_superior_release(&release, &previous.release_code)? && previous.id != existing.id
            {
                return Err(anyhow!(
                    "release {release} is not superior to active release {}",
                    previous.release_code
                ));
            }
        }
        existing.id
    } else if let Some(previous) = &previous {
        if !is_superior_release(&release, &previous.release_code)? {
            return Err(anyhow!(
                "release {release} is not superior to active release {}",
                previous.release_code
            ));
        }
        let previous_modules = db.modules_for_version(previous.id)?;
        if previous_modules
            .iter()
            .any(|module| !incoming_modules.contains(module))
        {
            db.clone_version(previous.id, &release)?
        } else {
            db.create_version(&release, false)?
        }
    } else {
        db.create_version(&release, false)?
    };
    for table in &mut tables {
        table.table_name = table.table_name.to_ascii_uppercase();
    }
    db.import_catalog(version_id, &tables, on_progress)?;
    if rebuild_search {
        db.rebuild_fts(version_id)?;
    }
    if activate {
        db.activate_version(version_id)?;
        if let Some(previous) = previous {
            if previous.id != version_id && is_superior_release(&release, &previous.release_code)? {
                db.delete_version_by_release(&previous.release_code)?;
            }
        }
    }
    Ok(version_id)
}

pub fn synchronize_cached_modules<F>(
    db: &Database,
    release: &str,
    modules: &[(OracleSource, &Path)],
    activate: bool,
    mut on_module: F,
) -> Result<i64>
where
    F: FnMut(&str, usize, usize),
{
    if modules.is_empty() {
        return Err(anyhow!("sync produced no modules"));
    }
    let previous = db.active_version()?;
    let mut version_id = None;
    for (source, cache_path) in modules {
        let tables = crate::sync_cache::read_path(source, cache_path)?;
        let table_count = tables.len();
        on_module(source.module.label(), 0, table_count);
        let current_id = synchronize_with_progress_internal(
            db,
            release,
            tables,
            false,
            |completed, total| on_module(source.module.label(), completed, total),
            false,
        )?;
        version_id = Some(current_id);
    }

    let version_id = version_id.ok_or_else(|| anyhow!("sync produced no modules"))?;
    db.rebuild_fts(version_id)?;
    if activate {
        db.activate_version(version_id)?;
        if let Some(previous) = previous {
            if previous.id != version_id && is_superior_release(release, &previous.release_code)? {
                db.delete_version_by_release(&previous.release_code)?;
            }
        }
    }
    Ok(version_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CatalogReference;

    fn catalog_table(module: &str, table_name: &str) -> CatalogTable {
        CatalogTable {
            module: module.to_owned(),
            table_name: table_name.to_owned(),
            ..CatalogTable::default()
        }
    }

    #[test]
    fn orders_oracle_releases() {
        assert!(is_superior_release("26C", "26B").expect("release"));
        assert!(is_superior_release("27A", "26C").expect("release"));
        assert!(!is_superior_release("26B", "26C").expect("release"));
        assert_eq!(
            parse_release_code("26B").expect("release"),
            ReleaseCode { year: 26, cycle: 1 }
        );
    }

    #[test]
    fn builds_hcm_help_center_source() {
        let source = OracleSource::help_center(OracleModule::Hcm, "26B").expect("HCM source");
        assert_eq!(
            source.index_url.as_str(),
            "https://docs.oracle.com/en/cloud/saas/human-resources/oedmh/index.html"
        );
        assert_eq!(OracleModule::Hcm.label(), "HCM");
    }

    #[test]
    fn rejects_non_superior_release() {
        let db = Database::in_memory().expect("in-memory SQLite");
        db.create_version("26C", true).expect("release");
        let error = synchronize(&db, "26B", vec![catalog_table("SCM", "OLDER_TABLE")], true)
            .expect_err("downgrade");
        assert!(error.to_string().contains("not superior"));
    }

    #[test]
    fn merges_missing_module_into_existing_release() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let version_id = db.create_version("26B", true).expect("release");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "SCM_TABLE"))
            .expect("SCM table");

        synchronize(
            &db,
            "26B",
            vec![catalog_table("FINANCIALS", "FINANCIALS_TABLE")],
            true,
        )
        .expect("module merge");

        let modules = db.modules_for_version(version_id).expect("modules");
        assert!(modules.contains("SCM"));
        assert!(modules.contains("FINANCIALS"));
    }

    #[test]
    fn prunes_previous_active_release_after_upgrade() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let version_id = db.create_version("26B", true).expect("release");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "OLD_TABLE"))
            .expect("old table");

        synchronize(&db, "26C", vec![catalog_table("SCM", "NEW_TABLE")], true).expect("upgrade");

        assert!(db.version_by_release("26B").expect("old release").is_none());
        assert!(db.version_by_release("26C").expect("new release").is_some());
    }

    #[test]
    fn keeps_previous_release_when_not_activating() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let version_id = db.create_version("26B", true).expect("release");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "OLD_TABLE"))
            .expect("old table");

        synchronize(&db, "26C", vec![catalog_table("SCM", "NEW_TABLE")], false)
            .expect("inactive upgrade");

        assert!(db.version_by_release("26B").expect("old release").is_some());
        assert!(
            !db.version_by_release("26C")
                .expect("new release")
                .expect("new version")
                .active
        );
    }

    #[test]
    fn reports_progress_for_each_imported_table() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let tables = vec![
            catalog_table("SCM", "FIRST_TABLE"),
            catalog_table("SCM", "SECOND_TABLE"),
        ];
        let mut progress = Vec::new();

        synchronize_with_progress(&db, "26B", tables, true, |completed, total| {
            progress.push((completed, total));
        })
        .expect("synchronize");

        assert_eq!(progress, vec![(1, 2), (2, 2)]);
    }

    #[test]
    fn parses_oracle_table_page() {
        let html = br#"
            <html>
              <head>
                <meta name="description" content="Receipt shipment lines">
              </head>
              <body>
                <h1><span class="chapterstart">RCV_SHIPMENT_LINES</span></h1>
                <table summary="Columns">
                  <tbody>
                    <tr>
                      <td>SHIPMENT_LINE_ID</td><td>NUMBER</td><td></td><td>18</td>
                      <td>Yes</td><td>Primary key</td><td></td>
                    </tr>
                  </tbody>
                </table>
                <table summary="Foreign Keys">
                  <tbody>
                    <tr>
                      <td>RCV_SHIPMENT_LINES</td><td>rcv_shipments</td>
                      <td>SHIPMENT_ID</td>
                    </tr>
                  </tbody>
                </table>
                <table summary="Indexes">
                  <tbody>
                    <tr>
                      <td>RCV_SHIPMENT_LINES_U1</td><td>Unique</td><td>Default</td>
                      <td>SHIPMENT_LINE_ID</td><td></td>
                    </tr>
                    <tr>
                      <td>RCV_SHIPMENT_LINES_U1</td><td>Unique</td><td>Default</td>
                      <td>LAST_UPDATE_DATE</td><td></td>
                    </tr>
                  </tbody>
                </table>
              </body>
            </html>
        "#;
        let source = OracleSource::help_center(OracleModule::Scm, "26B").expect("source");
        let page_url = Url::parse(
            "https://docs.oracle.com/en/cloud/saas/supply-chain-and-manufacturing/26b/oedsc/rcvshipmentlines-24402.html",
        )
        .expect("page URL");
        let table = parse_table_page(html, &page_url, &source)
            .expect("page parse")
            .expect("table page");

        assert_eq!(table.table_name, "RCV_SHIPMENT_LINES");
        assert_eq!(table.columns[0].column_name, "SHIPMENT_LINE_ID");
        assert_eq!(table.columns[0].length, Some(18));
        assert!(!table.columns[0].nullable);
        assert_eq!(table.references.len(), 1);
        assert_eq!(table.references[0].target_table, "RCV_SHIPMENTS");
        assert_eq!(table.references[0].source_column, "SHIPMENT_ID");
        assert_eq!(table.references[0].target_column, None);
        assert_eq!(table.indexes[0].index_name, "RCV_SHIPMENT_LINES_U1");
        assert!(table.indexes[0].is_unique);
        assert_eq!(
            table.indexes[0].indexed_columns,
            vec!["SHIPMENT_LINE_ID", "LAST_UPDATE_DATE"]
        );
        assert_eq!(table.object_type.as_deref(), Some("TABLE"));
    }

    #[test]
    fn reads_view_object_type_from_the_page() {
        let html = br#"
            <html><body>
              <p>Object type: VIEW</p>
              <h1><span class="chapterstart">ZX_LINES_V</span></h1>
              <table summary="Columns"><tbody>
                <tr><td>ROW_ID</td><td>VARCHAR2</td><td>18</td><td></td><td></td><td></td></tr>
              </tbody></table>
            </body></html>
        "#;
        let source = OracleSource::help_center(OracleModule::Financials, "26B").expect("source");
        let page_url = Url::parse(
            "https://docs.oracle.com/en/cloud/saas/financials/26b/oedmf/zxlinesv-1.html",
        )
        .expect("page URL");
        let table = parse_table_page(html, &page_url, &source)
            .expect("page parse")
            .expect("view page");
        assert_eq!(table.object_type.as_deref(), Some("VIEW"));
    }

    #[test]
    fn keeps_a_stated_object_type_other_than_view() {
        let html = br#"
            <html><body>
              <p>Object type: EXTERNAL</p>
              <h1><span class="chapterstart">ZX_LINES_V</span></h1>
              <table summary="Columns"><tbody>
                <tr><td>ROW_ID</td><td>VARCHAR2</td><td>18</td><td></td><td></td><td></td></tr>
              </tbody></table>
            </body></html>
        "#;
        let source = OracleSource::help_center(OracleModule::Financials, "26B").expect("source");
        let page_url = Url::parse(
            "https://docs.oracle.com/en/cloud/saas/financials/26b/oedmf/zxlinesv-1.html",
        )
        .expect("page URL");
        let table = parse_table_page(html, &page_url, &source)
            .expect("page parse")
            .expect("typed page");
        assert_eq!(table.object_type.as_deref(), Some("EXTERNAL"));
    }

    #[test]
    fn reads_same_guide_links_from_the_toc() {
        let html = br#"
            <a href="index.html">Index</a>
            <a href="glbalances-24959.html#glbalances-24959">GL_BALANCES</a>
            <a href="https://example.com/evil.html">outside</a>
        "#;
        let base = Url::parse("https://docs.oracle.com/en/cloud/saas/financials/26b/oedmf/toc.htm")
            .expect("toc");
        let links = toc_links(html, &base, "/en/cloud/saas/financials/26b/oedmf/").expect("links");
        assert_eq!(links.len(), 1);
        assert!(links[0].path().ends_with("/glbalances-24959.html"));
        assert!(links[0].fragment().is_none());
    }

    #[test]
    fn keeps_unsynced_modules_when_upgrading_one_module() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let version_id = db.create_version("26B", true).expect("release");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "OLD_SCM"))
            .expect("scm");
        db.upsert_catalog_table(version_id, &catalog_table("FINANCIALS", "OLD_FIN"))
            .expect("financials");

        synchronize(&db, "26C", vec![catalog_table("SCM", "NEW_SCM")], true)
            .expect("partial upgrade");

        let version = db.version_by_release("26C").expect("version").expect("26C");
        let modules = db.modules_for_version(version.id).expect("modules");
        assert!(modules.contains("SCM"));
        assert!(modules.contains("FINANCIALS"));
        assert!(db.version_by_release("26B").expect("old").is_none());
    }

    #[test]
    fn keeps_foreign_keys_from_an_unsynced_module_when_a_target_drops() {
        let db = Database::in_memory().expect("in-memory SQLite");
        let version_id = db.create_version("26B", true).expect("release");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "SCM_KEEP"))
            .expect("kept scm");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "SCM_DROP"))
            .expect("dropped scm");
        db.upsert_catalog_table(version_id, &catalog_table("SCM", "SCM_UNUSED"))
            .expect("unused scm");
        let mut financials = catalog_table("FINANCIALS", "FIN_HDR");
        financials.references = vec![
            CatalogReference {
                target_table: "SCM_KEEP".to_owned(),
                source_column: "KEEP_ID".to_owned(),
                target_column: None,
                constraint_name: None,
            },
            CatalogReference {
                target_table: "SCM_DROP".to_owned(),
                source_column: "DROP_ID".to_owned(),
                target_column: None,
                constraint_name: None,
            },
        ];
        db.upsert_catalog_table(version_id, &financials)
            .expect("financials");

        synchronize(&db, "26C", vec![catalog_table("SCM", "SCM_KEEP")], true).expect("upgrade");

        let kept = db.suggest_joins("FIN_HDR", "SCM_KEEP").expect("kept join");
        let dropped = db
            .suggest_joins("FIN_HDR", "SCM_DROP")
            .expect("dropped join");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source_column, "KEEP_ID");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].source_column, "DROP_ID");
        assert!(db.table_structure("SCM_UNUSED").expect("lookup").is_none());
    }
}
