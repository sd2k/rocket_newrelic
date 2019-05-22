# Rocket NewRelic

[![Build Status](https://travis-ci.org/sd2k/rocket_newrelic.svg?branch=master)](https://travis-ci.org/sd2k/rocket_newrelic)
[![docs.rs](https://docs.rs/rocket_newrelic/badge.svg)](https://docs.rs/rocket_newrelic)
[![crates.io](https://img.shields.io/crates/v/rocket_newrelic.svg)](https://crates.io/crates/rocket_newrelic)


A Rocket fairing instrumenting requests using New Relic.

Attach the fairing to your `Rocket` app, and any requests that include
a [`Transaction`] in their request guard will be instrumented using
the handler base path and name as the transaction name.

## Usage

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
