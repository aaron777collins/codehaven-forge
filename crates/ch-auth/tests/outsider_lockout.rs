//! ch-rx2: someone with no account who knows a username must not be able to lock that
//! person out of web sign-in, and password guessing must stay rate limited.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_auth::lockout::ACCOUNT_THRESHOLD;
use ch_core::model::*;
use ch_core::{Actor, Error};
use sqlx::PgPool;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_outsider_cannot_lock_the_owner_out_from_another_address(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "aaron", false).await;
    // The outsider hammers the username from one address, far past any threshold.
    for _ in 0..(ACCOUNT_THRESHOLD * 3) {
        let _ = c.state.credentials.sign_in("aaron", "outsider guess", &client("198.51.100.1")).await;
    }
    // The outsider's own address is slowed down, right password or not.
    let e = c.state.credentials.sign_in("aaron", PW, &client("198.51.100.1")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "{e:?}");
    // The owner, from their own address, still signs in, on the form and over Basic.
    c.state.credentials.sign_in("aaron", PW, &client("203.0.113.50")).await.unwrap();
    c.state.authn.from_basic("aaron", PW, &client("203.0.113.50")).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn guessing_from_many_addresses_is_still_limited_per_address(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "aaron", false).await;
    for i in 0..5 {
        for _ in 0..ACCOUNT_THRESHOLD {
            let _ = c.state.credentials.sign_in("aaron", "guess", &client(&format!("198.51.100.{i}"))).await;
        }
        let e = c.state.credentials.sign_in("aaron", PW, &client(&format!("198.51.100.{i}"))).await.unwrap_err();
        assert!(matches!(e, Error::RateLimited { .. }), "address {i}: {e:?}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_ipv6_network_shares_one_budget(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "aaron", false).await;
    // An attacker who owns a /64 can pick any address in it. They share one budget.
    for i in 0..ACCOUNT_THRESHOLD {
        let _ = c.state.credentials.sign_in("aaron", "guess", &client(&format!("2001:db8:1:2::{:x}", i + 1))).await;
    }
    let e = c.state.credentials.sign_in("aaron", PW, &client("2001:db8:1:2:aaaa::1")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "{e:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn admin_unlock_clears_every_address(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "aaron", false).await;
    for ip in ["198.51.100.1", "198.51.100.2"] {
        for _ in 0..ACCOUNT_THRESHOLD {
            let _ = c.state.credentials.sign_in("aaron", "guess", &client(ip)).await;
        }
    }
    c.state.accounts.unlock(&Actor::System, id, &ClientInfo::local()).await.unwrap();
    c.state.credentials.sign_in("aaron", PW, &client("198.51.100.1")).await.unwrap();
    c.state.credentials.sign_in("aaron", PW, &client("198.51.100.2")).await.unwrap();
}
