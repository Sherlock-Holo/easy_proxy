use std::io::Result;

use easy_proxy::run;

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}
