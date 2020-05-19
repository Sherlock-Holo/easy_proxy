use std::io::Result;

use easy_proxy::run;

#[async_std::main]
async fn main() -> Result<()> {
    run().await
}
