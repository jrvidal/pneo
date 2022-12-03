use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

pub struct Cache {
    connection: Connection,
    preprints: PathBuf,
}

impl Cache {
    pub fn new(connection: Connection, preprints: PathBuf) -> Self {
        Self {
            connection,
            preprints,
        }
    }

    pub fn init(&mut self) -> Result<()> {
        let _ = self
            .connection
            .execute(
                "CREATE TABLE IF NOT EXISTS eprints \
                (id TEXT, version INT, \
                    CONSTRAINT versions UNIQUE (id, version)\
                )",
                (),
            )
            .context("unable to create table")?;

        if let Err(error) = self.try_migrate_v1().context("unable to migrate downloads") {
            log::error!("{:?}", error);
        }

        Ok(())
    }

    #[deprecated]
    fn try_migrate_v1(&mut self) -> Result<()> {
        let entries: usize =
            self.connection
                .query_row("SELECT COUNT(*) FROM eprints", (), |row| row.get(0))?;

        if entries > 0 {
            return Ok(());
        }

        let files = std::fs::read_dir(&self.preprints)?;

        fn is_numeric_str(s: &str) -> bool {
            !s.is_empty() && s.chars().all(char::is_numeric)
        }

        let mut to_insert = vec![];

        for file in files.into_iter().filter_map(Result::ok) {
            let os_filename = file.file_name();

            let Some(filename) = os_filename.to_str()  else {
                continue;
            };

            if !filename.ends_with(".pdf") {
                continue;
            }

            let basename = &filename[..(filename.len() - ".pdf".len())];

            let Some(parts) = basename.split_once('.') else {
                continue;
            };

            if !is_numeric_str(parts.0) {
                continue;
            }

            let Some((suffix, version)) = parts.1.split_once('v') else {
                continue;
            };

            if !is_numeric_str(suffix) {
                continue;
            }

            let Ok(version )= u8::from_str_radix(version, 10) else {
                continue;
            };

            to_insert.push((format!("{}.{}", parts.0, suffix), version));
        }

        if to_insert.is_empty() {
            return Ok(());
        }

        log::info!("found {} entries to insert", to_insert.len());

        let tx = self.connection.transaction()?;

        for (id, version) in to_insert {
            let _ = tx.execute(
                "INSERT INTO eprints (id, version) VALUES (?, ?)",
                rusqlite::params![id, version],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    pub fn preprint_file_from_id(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut stmt = self.connection.prepare_cached(
            "SELECT version FROM eprints WHERE id = ? ORDER BY version DESC LIMIT 1",
        )?;

        let version: Option<u8> = stmt.query_row(&[id], |row| row.get(0)).optional()?;

        let Some(version) = version else {
            return Ok(None);
        };

        let filename = preprint::id_to_file(id, version);

        let filepath = {
            let mut path = self.preprints.clone();
            path.push(filename);
            path
        };

        if !std::path::Path::new(&filepath).exists() {
            anyhow::bail!("inconsistent cache, unable to find {:?}", filepath);
        }

        Ok(Some(filepath))
    }

    pub fn insert(
        &self,
        id: &str,
        referenced_id: &str,
        url: &str,
        content: Vec<u8>,
    ) -> Result<PathBuf> {
        let (basename, version) = preprint::validate(id, referenced_id, url)?;

        let path = {
            let mut path = self.preprints.clone();
            path.push(format!("{}.pdf", basename));
            path
        };

        std::fs::write(&path, content).context("unable to save preprint file")?;

        let mut stmt = self
            .connection
            .prepare_cached("INSERT INTO eprints (id, version) VALUES (?, ?)")?;

        let _ = stmt.execute(rusqlite::params![referenced_id, version])?;

        Ok(path)
    }

    pub fn get_downloaded(&self) -> Result<HashSet<String>> {
        self.do_get_downloaded()
    }

    fn do_get_downloaded(&self) -> Result<HashSet<String>> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT DISTINCT id FROM eprints")?;

        let ids: HashSet<String> = stmt
            .query_map((), |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        Ok(ids)
    }
}

mod preprint {
    use anyhow::Context;

    use super::Result;

    pub fn id_to_file(id: &str, version: u8) -> String {
        let basename = id.rsplit_once("/").map(|split| split.1).unwrap_or(id);

        format!("{}v{}.pdf", basename, version)
    }

    /// returns a (basename, version) tuple
    pub fn validate<'a>(
        id: &'a str,
        referenced_id: &'a str,
        url: &'a str,
    ) -> Result<(&'a str, u8)> {
        const ID_PREFIX: &str = "http://arxiv.org/abs/";

        let identifier = id
            .split_once(ID_PREFIX)
            .filter(|(prefix, _)| *prefix == "")
            .map(|(_, suffix)| suffix)
            .ok_or_else(|| anyhow::anyhow!("unexpected arxiv id {:?}", id))?;

        let (identifier, version) = identifier
            .rsplit_once("v")
            .map(|(prefix, suffix)| (prefix, Some(suffix)))
            .unwrap_or((identifier, None));

        if identifier != referenced_id {
            anyhow::bail!(
                "inconsistent ids. arxiv = {:?} external reference = {:?}",
                identifier,
                referenced_id
            );
        }

        let (basename, url_version) = {
            let index = url
                .rfind("/")
                .filter(|&idx| idx < url.len() - 1)
                .ok_or_else(|| anyhow::anyhow!("unexpected url structure {:?}", url))?;

            let basename = &url[(index + 1)..];

            let version = basename
                .rsplit_once("v")
                .map(|(_, suffix)| suffix)
                .ok_or_else(|| anyhow::anyhow!("no version suffix in {:?}", url))?;

            (basename, version)
        };

        let same_version = match (version, url_version) {
            (Some(version), url_version) => version == url_version,
            (None, _) => true,
        };

        if !same_version {
            anyhow::bail!(
                "inconsistent versions. id = {:?} ({:?}) url = {:?} ({})",
                id,
                version,
                url,
                url_version
            );
        }

        let version = u8::from_str_radix(url_version, 10)
            .with_context(|| format!("invalid version string {:?}", url_version))?;

        Ok((basename, version))
    }
}
