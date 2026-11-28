#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), barn_luau_stall::MainError> {
    barn_luau_stall::run().await
}
