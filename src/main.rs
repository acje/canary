use actix_web::{App, HttpServer, Responder, get, middleware};
use canary::exit::ExitCode;
use canary::port::ListenPort;
use std::net::SocketAddr;

#[actix_web::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let listen = match ListenPort::from_env() {
        Ok(listen) => listen,
        Err(error) => {
            eprintln!("canary: {error}");
            std::process::exit(ExitCode::Config.code());
        }
    };

    let address = SocketAddr::from(([0, 0, 0, 0], listen.port()));
    let server = match HttpServer::new(|| {
        App::new()
            .wrap(middleware::Logger::default())
            .service(chirp)
    })
    .bind(address)
    {
        Ok(server) => server,
        Err(error) => {
            eprintln!("canary: cannot bind {address}: {error}");
            std::process::exit(ExitCode::Config.code());
        }
    };

    if let Err(error) = server.run().await {
        eprintln!("canary: server failed: {error}");
        std::process::exit(ExitCode::Software.code());
    }
}

#[get("/")]
async fn chirp() -> impl Responder {
    "Canary 0.50.0 is alive!\r\n"
}
