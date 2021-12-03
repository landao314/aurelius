use std::error::Error;
use std::time::Duration;

use aurelius::r#async::Server;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let socket = "127.0.0.1:0".parse().unwrap();
    let server = Server::bind(&socket).await?;

    println!("http://{}", server.addr());

    loop {
        server.send("foo");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    Ok(())
}
