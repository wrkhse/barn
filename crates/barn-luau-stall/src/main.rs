use barn_luau_stall::MainError;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), MainError> {
    barn_luau_stall::run().await
}
