use actix_web::{App, HttpServer, Responder, get, middleware};
use canary::exit::ExitCode;
use canary::port::ListenPort;
use std::net::SocketAddr;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let listen = match ListenPort::from_env() {
        Ok(listen) => listen,
        Err(error) => {
            eprintln!("canary: {error}");
            std::process::exit(ExitCode::Config.code());
        }
    };

    HttpServer::new(|| {
        App::new()
            .wrap(middleware::Logger::default())
            .service(chirp)
    })
    .bind(SocketAddr::from(([0, 0, 0, 0], listen.port())))?
    .run()
    .await
}

#[get("/")]
async fn chirp() -> impl Responder {
    "Canary 0.50.0 is alive!\r\n"
}
