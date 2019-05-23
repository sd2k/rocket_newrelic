/*!
A Rocket fairing instrumenting requests using New Relic.

Attach the fairing to your `Rocket` app, and any requests that include
a [`Transaction`] in their request guard will be instrumented using
the handler base path and name as the transaction name.

## Usage

**Important** - this fairing still requires the [New Relic daemon] to be run
alongside your app in some way, and the underlying [newrelic] and
[newrelic-sys] crates have some additional build requirements. Make sure these
are met when trying to use this crate.

Crucially the `libnewrelic` C SDK requires a few functions not provided by musl
(at least `qsort_r` and `backtrace`), so this won't (currently) build against
musl.

---

Add the crate to your Cargo.toml:

```toml
[dependencies]
rocket_newrelic = { git = "https://github.com/sd2k/rocket_newrelic" }
```

Then add a `&Transaction` request guard to any handlers you
wish to instrument:

```rust
use rocket_newrelic::Transaction;

#[get("/user/me")]
pub fn get_me(_transaction: &Transaction) -> &'static str {
    "It's me!"
}
```

Finally, attach the fairing to your `Rocket` app:

```rust
use rocket_newrelic::NewRelic;

fn main() -> {
    let newrelic = NewRelic::new("MY_APP_NAME", "MY_LICENSE_KEY")
        .expect("Could not register with New Relic");
    rocket::ignite()
        .manage(newrelic)
        .mount("/root", routes![get_me])
        .launch();
}
```

In the above example we'd then be able to see these transactions under
`/root/get_me`.

## Advanced usage

The [`Transaction`] object used in the request guard provides a few methods
to allow further instrumentation, such as custom attributes and transaction
segments. The below example demonstrates some of this functionality; see the
methods of `Transaction` for more details.

```rust
#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate rocket;

use newrelic::{Datastore, ExternalParamsBuilder};
use rocket_contrib::json::Json;
use rocket_newrelic::{NewRelic, Transaction};
use serde_json::Value;

struct User;

// This would normally connect to a database and perhaps return some data.
fn insert_into_db(_: &Json<Value>) -> Result<User, ()> {
    Ok(User {})
}

#[post("/users", data = "<user>")]
fn create_user(transaction: &Transaction, user: Json<Value>) {
    // Add attributes to a transaction
    if let Some(Value::String(name)) = user.get("name") {
        transaction.add_attribute("user name", name);
    }
    if let Some(Some(age)) = user.get("age").map(|a| a.as_i64()) {
        transaction.add_attribute("user age", age);
    }

    // Executing a query in a datastore segment
    let query = "INSERT INTO users VALUES (%s, %s);";
    match transaction.datastore_segment(Datastore::Postgres, "users", "insert", query, |_| {
        insert_into_db(&user)
    }) {
        Ok(_) => println!("Created user"),
        Err(_) => println!("Could not create user"),
    }

    // Doing expensive operations in a custom segment
    let _expensive_value: Result<reqwest::Response, reqwest::Error> =
        transaction.custom_segment("process user", "process", |s| {
            // Nesting an external segment within the custom segment
            let url = "https://logging-thing";
            let external_params = ExternalParamsBuilder::new(url)
                .procedure("set")
                .library("reqwest")
                .build()
                .unwrap();
            s.external_nested(&external_params, |_| {
                reqwest::Client::new().post(url).send()
            })
        });
}

fn main() {
    let newrelic = NewRelic::from_env();
    rocket::ignite()
        .manage(newrelic)
        .mount("/", routes![create_user])
        .launch();
}
```

### Diesel queries

With the `diesel` feature enabled it's possible to pass a Diesel query,
along with a `&Connection`, into the `diesel_segment_load` and
`diesel_segment_first` methods of a [`Transaction`]. This will log the SQL query
and return either all results, or the first result, respectively.

[Rocket]: rocket::Rocket
[Transaction]: crate::Transaction
[newrelic]: https://github.com/sd2k/newrelic
[newrelic-sys]: https://github.com/sd2k/newrelic-sys
[New Relic daemon]: https://docs.newrelic.com/docs/agents/c-sdk/get-started/introduction-c-sdk#architecture
*/
#![deny(missing_docs)]
use std::{
    env,
    sync::{Arc, RwLock},
};

use log::{debug, info, warn};
use rocket::{
    fairing::{Fairing, Info, Kind},
    request::{self, FromRequest},
    Data, Outcome, Request, Response,
};

mod error {
    use newrelic::Error as NewRelicError;
    use std::{env::VarError, fmt};

    #[derive(Debug)]
    pub enum Error {
        NewRelicError(NewRelicError),
        VarError(VarError),
    }

    impl From<NewRelicError> for Error {
        fn from(other: NewRelicError) -> Self {
            Error::NewRelicError(other)
        }
    }

    impl From<VarError> for Error {
        fn from(other: VarError) -> Self {
            Error::VarError(other)
        }
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Error::NewRelicError(e) => write!(f, "{}", e),
                Error::VarError(e) => write!(f, "{}", e),
            }
        }
    }

    impl std::error::Error for Error {}
}

#[must_use]
/// A Rocket fairing which instruments requests using New Relic.
///
/// See the library documentation for more details on usage.
pub struct NewRelic(Arc<newrelic::App>);

impl NewRelic {
    /// Create a new New Relic fairing with the default New Relic SDK settings.
    pub fn new(app_name: &str, license_key: &str) -> Result<Self, error::Error> {
        // Register application with New Relic
        match newrelic::App::new(app_name, license_key) {
            Ok(app) => {
                info!("Registered with New Relic using app name {}", app_name);
                Ok(NewRelic(Arc::new(app)))
            }
            Err(e) => {
                warn!("Failed to register with New Relic: {}", e);
                Err(e.into())
            }
        }
    }

    /// Create a New Relic fairing with some custom New Relic SDK configuration.
    ///
    /// This allows settings such as the SDK log level and destination,
    /// timeout, and daemon socket to be configured.
    pub fn with_config(
        app_name: &str,
        license_key: &str,
        config: newrelic::NewRelicConfig,
    ) -> Result<Self, error::Error> {
        config.init()?;
        Self::new(app_name, license_key)
    }

    /// Create a New Relic fairing, fetching config from the environment.
    ///
    /// The following environment variables are used:
    ///
    /// Required:
    ///
    /// - `NEW_RELIC_APP_NAME`
    /// - `NEW_RELIC_LICENSE_KEY`
    ///
    /// Optional
    ///
    /// - `NEW_RELIC_LOG_LEVEL` - must be able to be parsed to a `log::Level`.
    pub fn from_env() -> Result<Self, error::Error> {
        let app_name = env::var("NEW_RELIC_APP_NAME")?;
        let license_key = env::var("NEW_RELIC_LICENSE_KEY")?;

        let log_level = env::var("NEW_RELIC_LOG_LEVEL");
        if let Ok(level) = log_level {
            let level = match level.parse() {
                Ok(level) => level,
                Err(_) => {
                    warn!("Invalid value for NEW_RELIC_LOG_LEVEL; defaulting to Info");
                    log::Level::Info
                }
            };
            newrelic::NewRelicConfig::default()
                .logging(level, newrelic::LogOutput::StdErr)
                .init()?;
        }
        Self::new(&app_name, &license_key)
    }
}

impl Fairing for NewRelic {
    fn info(&self) -> Info {
        Info {
            name: "New Relic instrumentation",
            kind: Kind::Request | Kind::Response,
        }
    }

    /// Store an atomic reference to the app in the request-local cache,
    /// so that it can be used to create a transaction if required.
    fn on_request(&self, request: &mut Request, _: &Data) {
        request.local_cache(|| AppWrapper::App(Arc::clone(&self.0)));
    }

    /// End the New Relic transaction, if the request has one stored.
    ///
    /// Also adds an error code to the transaction if the response did
    /// not succeed.
    fn on_response(&self, request: &Request, response: &mut Response) {
        if let Transaction::Running(inner) = request.local_cache(|| Transaction::None) {
            match inner.0.write() {
                Ok(ref mut t) => {
                    // Record any errors
                    let status = response.status();
                    if !status.class().is_success() {
                        if let Err(msg) = t.notice_error(100, &status.to_string(), "") {
                            warn!("Could not add error to New Relic transaction: {}", msg);
                        }
                    }
                    // End the transaction explicitly here.
                    // Otherwise it ends after the response has finished being
                    // sent to the client, when it's dropped.
                    t.end();
                }
                _ => {
                    warn!("Could not lock mutex to end transaction");
                }
            };
        }
    }
}

/// This is used to pass the app into the request-local cache. This
/// is needed
///
/// We need to use an `Arc<App>` here because the request-local cache requires
/// that any references are `'static` (see https://github.com/SergioBenitez/Rocket/issues/1005).
/// `App` isn't Clone or Copy since it contains a raw pointer to some C memory
/// so we reference-count instead.
enum AppWrapper {
    App(Arc<newrelic::App>),
    None,
}

/// This has to be public since it's used inside the Transaction enum,
/// but it serves no purpose to users (since its inner field is private).
#[doc(hidden)]
pub struct InnerTransaction(RwLock<newrelic::Transaction>);

/// A New Relic transaction.
///
/// When included in a request guard, this transaction will trace
/// the request in New Relic. Custom attributes and segments can be
/// added using the various methods of this enum.
///
/// Note that if an error is encountered when registering the transaction
/// with the New Relic SDK then this could be the `Transaction::None`
/// variant, indicating that the request is not being instrumented.
/// In this case a warning message will be logged.
pub enum Transaction {
    /// A running New Relic transaction.
    Running(InnerTransaction),

    /// A dummy transaction; used if the New Relic SDK
    /// returns an error.
    None,
}

impl Transaction {
    /// Create a new transaction for a request.
    ///
    /// The New Relic transaction will have the URL and transaction name
    /// attributes set.
    fn new(app: &newrelic::App, request: &Request) -> Self {
        // Use the route handler as the transaction name.
        // This should always be used inside a request guard so that
        // request.route() is not None.
        let transaction_name: String = request
            .route()
            .map(|r| {
                format!(
                    "{}/{}",
                    r.base.to_string().trim_start_matches('/'),
                    r.name.unwrap_or("unknown_handler")
                )
            })
            .unwrap_or_else(|| "unknown_handler".to_string());

        app.web_transaction(&transaction_name)
            .map(|transaction| {
                debug!("Began New Relic transaction");
                if let Err(e) = transaction.add_attribute("uri", &request.uri().to_string()) {
                    warn!("Could not add uri attribute to transaction: {}", e);
                };
                Transaction::Running(InnerTransaction(RwLock::new(transaction)))
            })
            .unwrap_or_else(|e| {
                warn!("Error beginning New Relic transaction: {}", e);
                Transaction::None
            })
    }

    /// Add an attribute to the transaction.
    pub fn add_attribute<'a, T>(&self, key: &str, attribute: T)
    where
        T: Into<newrelic::Attribute<'a>>,
    {
        if let Transaction::Running(inner) = self {
            match inner.0.read() {
                Ok(t) => {
                    match t.add_attribute(key, attribute) {
                        Ok(_) => debug!("Successfully added attribute"),
                        Err(e) => debug!("Could not add attribute to transaction: {}", e),
                    };
                }
                Err(e) => {
                    warn!("Error locking transaction RwLock: {}", e);
                }
            };
        }
    }

    /// Execute the function in a named custom segment.
    ///
    /// `func` should be a function taking a `newrelic::Segment`. This allows
    /// nested segments to be created using methods of the passed segment.
    ///
    /// If the current transaction could not be registered, this just calls the
    /// given function outside of a segment.
    #[inline(always)]
    pub fn custom_segment<F, V>(&self, name: &str, category: &str, func: F) -> V
    where
        F: FnOnce(newrelic::Segment) -> V,
    {
        match self {
            Transaction::Running(inner) => match inner.0.read() {
                Ok(t) => t.custom_segment(name, category, func),
                Err(e) => {
                    warn!("Error locking transaction RwLock: {}", e);
                    func(Default::default())
                }
            },
            Transaction::None => func(Default::default()),
        }
    }

    /// Execute the function in a datastore segment.
    ///
    /// `func` should be a function taking a `newrelic::Segment`. This allows
    /// nested segments to be created using methods of the passed segment.
    ///
    /// The `table` argument should not contain any slash characters.
    ///
    /// If the current transaction could not be registered, this just calls the
    /// given function with a `newrelic::Segment::None`.
    ///
    /// See `newrelic::DatastoreParamsBuilder` and
    /// `newrelic::Transaction::datastore_segment` for more details.
    ///
    /// SQL Obfuscation
    /// ---------------
    ///
    /// The supplied SQL string will go through the New Relic SDK's
    /// basic literal replacement obfuscator that strips the SQL
    /// string literals (values between single or double quotes) and
    /// numeric sequences, replacing them with the ? character.
    /// For example:
    ///
    /// This SQL:
    ///      SELECT * FROM table WHERE ssn=‘000-00-0000’
    ///
    /// obfuscates to:
    ///      SELECT * FROM table WHERE ssn=?
    ///
    /// Because the default obfuscator just replaces literals, there
    /// could be cases that it does not handle well. For instance, it
    /// will not strip out comments from your SQL string, it will not
    /// handle certain database-specific language features, and it
    /// could fail for other complex cases.
    #[inline(always)]
    pub fn datastore_segment<F, V>(
        &self,
        datastore: newrelic::Datastore,
        table: &str,
        operation: &str,
        sql: &str,
        func: F,
    ) -> V
    where
        F: FnOnce(newrelic::Segment) -> V,
    {
        match self {
            Transaction::Running(inner) => match inner.0.read() {
                Ok(t) => {
                    let params = newrelic::DatastoreParamsBuilder::new(datastore)
                        .collection(table)
                        .operation(operation)
                        .query(sql)
                        .build();
                    match params {
                        Ok(p) => t.datastore_segment(&p, func),
                        Err(e) => {
                            warn!("Error building datastore parameters: {}", e);
                            func(Default::default())
                        }
                    }
                }
                Err(e) => {
                    warn!("Error locking transaction RwLock: {}", e);
                    func(Default::default())
                }
            },
            Transaction::None => func(Default::default()),
        }
    }

    /// Execute a function in an external segment.
    ///
    /// `func` should be a function taking a `newrelic::Segment`. This allows
    /// nested segments to be created using methods of the passed segment.
    ///
    /// The `procedure` and `library` arguments, if provided, should not
    /// contain any slash characters.
    ///
    /// If the current transaction could not be registered, this just calls the
    /// given function with a `newrelic::Segment::None`.
    ///
    /// See `newrelic::ExternalParamsBuilder` and
    /// `newrelic::Transaction::external_segment` for more details.
    #[inline(always)]
    pub fn external_segment<F, V>(
        &self,
        host: &str,
        procedure: Option<&str>,
        library: Option<&str>,
        func: F,
    ) -> V
    where
        F: FnOnce(newrelic::Segment) -> V,
    {
        match self {
            Transaction::Running(inner) => match inner.0.read() {
                Ok(t) => {
                    let mut params = newrelic::ExternalParamsBuilder::new(host);
                    if let Some(p) = procedure {
                        params = params.procedure(p);
                    }
                    if let Some(l) = library {
                        params = params.library(l);
                    }
                    match params.build() {
                        Ok(p) => t.external_segment(&p, func),
                        Err(e) => {
                            warn!("Error building external New Relic parameters: {}", e);
                            func(Default::default())
                        }
                    }
                }
                Err(e) => {
                    warn!("Error locking transaction RwLock: {}", e);
                    func(Default::default())
                }
            },
            Transaction::None => func(Default::default()),
        }
    }
}

impl<'a, 'r> FromRequest<'a, 'r> for &'a Transaction {
    type Error = ();

    // Begin the New Relic transaction here. This implies that ONLY requests
    // which include a Transaction in their request guards will be traced.
    // Note that this will only produce a valid transaction if the NewRelic
    // fairing has been attached.
    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, Self::Error> {
        let transaction = match request.local_cache(|| AppWrapper::None) {
            AppWrapper::App(ref app) => request.local_cache(|| Transaction::new(app, request)),
            AppWrapper::None => request.local_cache(|| Transaction::None),
        };
        Outcome::Success(transaction)
    }
}

#[cfg(feature = "diesel")]
mod diesel {
    use diesel::{
        backend::Backend,
        dsl::Limit,
        prelude::*,
        query_builder::{debug_query, QueryFragment},
        query_dsl::{methods::LimitDsl, LoadQuery},
    };
    use log::warn;

    use super::Transaction;

    impl Transaction {
        /// Execute a Diesel query in a datastore segment,  returning the first row.
        ///
        /// See `Transaction::datastore_segment` for more details.
        ///
        /// *Note*: requires the `diesel` feature.
        pub fn diesel_segment_first<T, Conn, B, V>(
            &self,
            datastore: newrelic::Datastore,
            table: &str,
            query: T,
            conn: &Conn,
        ) -> QueryResult<V>
        where
            T: LimitDsl + QueryFragment<B> + RunQueryDsl<Conn>,
            B: Backend,
            B::QueryBuilder: Default,
            Limit<T>: LoadQuery<Conn, V>,
        {
            match self {
                Transaction::Running(inner) => match inner.0.read() {
                    Ok(t) => {
                        let sql = debug_query(&query).to_string();
                        let params = newrelic::DatastoreParamsBuilder::new(datastore)
                            .collection(table)
                            .operation("select")
                            .query(&sql)
                            .build();
                        match params {
                            Ok(p) => t.datastore_segment(&p, |_| query.first(conn)),
                            Err(e) => {
                                warn!("Error building New Relic datastore parameters: {}", e);
                                query.first(conn)
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error locking transaction RwLock: {}", e);
                        func(Default::default())
                    }
                },
                Transaction::None => query.first(conn),
            }
        }

        /// Execute a Diesel query in a datastore segment, loading the results.
        ///
        /// See `Transaction::datastore_segment` for more details.
        ///
        /// *Note*: requires the `diesel` feature.
        pub fn diesel_segment_load<T, Conn, B, V>(
            &self,
            datastore: newrelic::Datastore,
            table: &str,
            query: T,
            conn: &Conn,
        ) -> QueryResult<Vec<V>>
        where
            T: LoadQuery<Conn, V> + QueryFragment<B>,
            B: Backend,
            B::QueryBuilder: Default,
        {
            match self {
                Transaction::Running(inner) => match inner.0.read() {
                    Ok(t) => {
                        let t = inner.0.read().expect("Mutex problem");
                        let sql = debug_query(&query).to_string();
                        let params = newrelic::DatastoreParamsBuilder::new(datastore)
                            .collection(table)
                            .operation("select")
                            .query(&sql)
                            .build();
                        match params {
                            Ok(p) => t.datastore_segment(&p, |_| query.load(conn)),
                            Err(e) => {
                                warn!("Error building New Relic datastore parameters: {}", e);
                                query.load(conn)
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error locking transaction RwLock: {}", e);
                        func(Default::default())
                    }
                },
                Transaction::None => query.load(conn),
            }
        }
    }
}
