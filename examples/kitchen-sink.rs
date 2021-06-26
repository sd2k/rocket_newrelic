#[macro_use]
extern crate rocket;

use newrelic::{Datastore, ExternalParamsBuilder};
use rocket::serde::json::Json;
use rocket_newrelic::{NewRelic, Transaction};
use serde_json::Value;

struct User;

// This would normally connect to a database and perhaps return some data.
fn insert_into_db(_: &Json<Value>) -> Result<User, ()> {
    Ok(User {})
}

#[post("/users", data = "<user>")]
async fn create_user(transaction: &Transaction, user: Json<Value>) {
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
    let _expensive_value: Result<reqwest::Response, reqwest::Error> = transaction
        .custom_segment("process user", "process", |s| {
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
        })
        .await;
}

#[launch]
fn launch() -> _ {
    let newrelic = NewRelic::from_env();
    rocket::build()
        .manage(newrelic)
        .mount("/", routes![create_user])
}
