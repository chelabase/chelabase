mod support;

use support::*;
use auth::zitadel::{NewUser, ZError, Zitadel};

async fn setup() -> (MockZitadel, Zitadel) {
    let mock = MockZitadel::start().await;
    let client = Zitadel::new(&mock.base, TOKEN);
    (mock, client)
}

#[tokio::test]
async fn reads_whether_a_sign_in_asked_for_sign_up() {
    let (mock, z) = setup().await;
    assert!(z.auth_request(&mock.add_auth_request(true)).await.unwrap());
    assert!(!z.auth_request(&mock.add_auth_request(false)).await.unwrap());
    assert!(matches!(
        z.auth_request("V2_missing").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

#[tokio::test]
async fn signs_in_and_finishes() {
    let (mock, z) = setup().await;
    let user = mock.add_user("ada@example.com", "Secret-pass1", true);
    assert_eq!(
        z.find_user_by_email("o1", "ADA@example.com").await.unwrap(),
        Some(user.clone())
    );
    assert_eq!(
        z.find_user_by_email("o1", "nobody@example.com")
            .await
            .unwrap(),
        None
    );
    assert!(z.email_verified(&user).await.unwrap());
    let session = z.create_session(&user, "Secret-pass1").await.unwrap();
    let ar = mock.add_auth_request(false);
    assert!(
        z.finish(&ar, &session)
            .await
            .unwrap()
            .starts_with("http://localhost:3000/callback?code=")
    );
    assert!(matches!(
        z.create_session(&user, "wrong").await,
        Err(ZError::Rejected { status: 400, .. })
    ));
}

#[tokio::test]
async fn creates_and_verifies_a_user() {
    let (mock, z) = setup().await;
    let new = NewUser {
        org_id: "o1",
        email: "bob@example.com",
        given_name: "Bob",
        family_name: "B",
        password: "Secret-pass1",
        verified: false,
        url_template: "http://localhost:3000/verify?code={{.Code}}",
    };
    let id = z.create_user(&new).await.unwrap();
    assert_eq!(mock.emails(), vec![(id.clone(), "verify".to_string())]);
    assert!(matches!(
        z.create_user(&new).await,
        Err(ZError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        z.verify_email(&id, "000000").await,
        Err(ZError::Rejected { status: 400, .. })
    ));
    z.verify_email(&id, EMAIL_CODE).await.unwrap();
    assert!(z.email_verified(&id).await.unwrap());
}

#[tokio::test]
async fn resets_a_password() {
    let (mock, z) = setup().await;
    let id = mock.add_user("cy@example.com", "Old-pass-1", true);
    z.send_password_reset(
        &id,
        "http://localhost:3000/password-reset?userId={{.UserID}}&code={{.Code}}",
    )
    .await
    .unwrap();
    assert_eq!(mock.emails(), vec![(id.clone(), "reset".to_string())]);
    match z.set_password(&id, RESET_CODE, "short").await {
        Err(ZError::Rejected {
            status: 400,
            message,
        }) => assert!(message.contains("Password")),
        other => panic!("{other:?}"),
    }
    z.set_password(&id, RESET_CODE, "New-pass-12")
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unreachable_or_failing_zitadel_is_unavailable() {
    let (mock, z) = setup().await;
    mock.set_down(true);
    assert!(matches!(
        z.auth_request("V2_1").await,
        Err(ZError::Unavailable(_))
    ));
    let nothing = Zitadel::new("http://127.0.0.1:9", TOKEN);
    assert!(matches!(
        nothing.auth_request("V2_1").await,
        Err(ZError::Unavailable(_))
    ));
}
