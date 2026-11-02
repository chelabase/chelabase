// The managed pool hands out no connection, so nothing else runs on it and no role session
// can be opened from it.
async fn f(pool: &chelabase_pgsession::ServicePool<chelabase_pgsession::InternalSource>) {
    let _ = pool.acquire(uuid::Uuid::nil(), "main").await;
}

async fn g(pool: &chelabase_pgsession::ServicePool<chelabase_pgsession::InternalSource>) {
    let _ = pool.pools.acquire(uuid::Uuid::nil(), "main").await;
}

fn main() {}
