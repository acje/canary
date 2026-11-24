#![warn(clippy::pedantic)]
use actix_web::{App, HttpServer, Responder, get, middleware};
use std::env;
use std::net::SocketAddr;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    HttpServer::new(|| {
        App::new()
            .wrap(middleware::Logger::default())
            .service(chirp)
    })
    .bind(SocketAddr::from(([0, 0, 0, 0], get_server_port())))?
    .run()
    .await
}

#[get("/")]
async fn chirp() -> impl Responder {
    "Canary 0.50.0 is alive!\r\n"
}

fn get_server_port() -> u16 {
    match env::var("PORT").ok().and_then(|port| port.parse().ok()) {
        Some(port) => port,
        None => abort_missing_port(),
    }
}

fn abort_missing_port() -> ! {
    eprintln!("Aborting: Failed to read environment variable PORT");
    std::process::exit(78);
}
