#![warn(clippy::pedantic)]
use actix_web::{get, middleware, App, HttpServer, Responder};
use std::env;
use std::net::SocketAddr;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "actix_web=info");
    env_logger::init();

    HttpServer::new(|| {
        App::new()
            .wrap(middleware::Logger::default()) // enable logger
            .service(chirp)
    })
    .bind(SocketAddr::from(([0, 0, 0, 0], get_server_port())))?
    .run()
    .await
}

#[get("/")]
async fn chirp() -> impl Responder {
    "Canary 0.46.0 is alive!\r\n"
}

// Get the port number to listen on or fail fast.
fn get_server_port() -> u16 {
    env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .expect("Aborting: Failed to read environment variable PORT")
}
