use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, ToSql};

enum Message {
    Compute(Box<dyn FnOnce() -> Result<()> + Send>),
    End,
}

pub struct StoreConnection {
    connection: Arc<Mutex<Connection>>,
    tx: mpsc::Sender<Message>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StoreConnection {
    pub fn start(connection: Connection) -> Self {
        let (tx, rx) = mpsc::channel();

        let handle = std::thread::spawn(move || loop {
            let Ok(msg): Result<Message, _> = rx.recv() else {
                break;
            };

            let Message::Compute(fun) = msg else {
                break;
            };

            if let Err(error) = fun() {
                log::error!("{:?}", error);
            }
        });

        Self {
            tx,
            connection: Arc::new(Mutex::new(connection)),
            handle: Some(handle),
        }
    }

    fn end(&mut self) {
        let _ = self.tx.send(Message::End);

        if let Err(err) = self.handle.take().unwrap().join() {
            log::error!("error closing store {:?}", err);
        }
    }

    pub fn execute(&self, fun: impl FnOnce(Store) -> Result<()> + Send + 'static) {
        let connection = self.connection.clone();

        let _ = self.tx.send(Message::Compute(Box::new(move || {
            let mut mutex_guard = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned lock"))?;

            let store = Store {
                connection: &mut *(mutex_guard),
            };

            fun(store)
        })));
    }
}

impl Drop for StoreConnection {
    fn drop(&mut self) {
        self.end();
    }
}

pub struct Store<'a> {
    connection: &'a mut Connection,
}

pub struct Record {
    pub control_number: u32,
    pub title: String,
    pub authors: Vec<String>,
    pub created: String,
}

struct RawRecord {
    pub control_number: u32,
    pub title: String,
    pub authors: String,
    pub created: String,
}

impl TryFrom<&'_ rusqlite::Row<'_>> for RawRecord {
    type Error = rusqlite::Error;

    fn try_from(row: &'_ rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawRecord {
            control_number: row.get("control_number")?,
            title: row.get("title")?,
            created: row.get("created")?,
            authors: row.get("authors")?,
        })
    }
}

impl TryFrom<RawRecord> for Record {
    type Error = serde_json::Error;

    fn try_from(value: RawRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            control_number: value.control_number,
            title: value.title,
            authors: serde_json::from_str(&value.authors)?,
            created: value.created,
        })
    }
}

impl Record {
    fn authors_row(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.authors)?)
    }
}

struct Query {
    pub title: Option<String>,
}

impl<'a> Store<'a> {
    pub fn init(&self) -> Result<()> {
        self.connection.execute(
            r#"
                CREATE TABLE IF NOT EXISTS records
                (control_number INT NOT NULL, version INT DEFAULT 1, title TEXT NOT NULL, authors TEXT NOT NULL, created TEXT NOT NULL,
                    CONSTRAINT identifier UNIQUE (control_number)
                )
            "#,
            (),
        )?;

        Ok(())
    }

    pub fn upsert(&mut self, records: Vec<Record>) -> Result<()> {
        let tx = self.connection.transaction()?;

        let mut stmt = tx.prepare_cached(
            r#"
                INSERT INTO records (control_number, title, authors, created) VALUES (?, ?, ?, ?)
                ON CONFLICT (control_number) DO UPDATE SET
                    title=excluded.title, authors=excluded.authors, created=excluded.created
            "#,
        )?;

        for record in records {
            stmt.execute(rusqlite::params![
                record.control_number,
                record.title,
                record.authors_row()?,
                record.created
            ])?;
        }

        drop(stmt);
        tx.commit()?;

        Ok(())
    }

    pub fn query(&self, query: Query) -> Result<Vec<Record>> {
        let mut stmt = "SELECT * FROM records".to_string();
        let mut params = vec![];

        if let Some(title) = query.title.as_ref() {
            stmt.push_str(" WHERE title = ?");
            params.push(title as &dyn ToSql);
        }

        stmt.push_str(" LIMIT 50");

        let mut stmt = self.connection.prepare(&stmt)?;

        let mapped_rows = stmt.query_map(&params[..], |row| RawRecord::try_from(row))?;

        let result: Vec<_> = mapped_rows
            .map(|raw| raw.map_err(anyhow::Error::from))
            .map(|raw| raw.and_then(|raw| Ok(Record::try_from(raw)?)))
            .collect::<Result<_, _>>()?;

        Ok(result)
    }
}
