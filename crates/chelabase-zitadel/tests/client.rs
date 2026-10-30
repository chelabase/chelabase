use chelabase_zitadel::client::{EmailMatch, NewUser, UserState, ZError, Zitadel};
use chelabase_zitadel::mock::*;

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
            .starts_with("http://studio.chelabase.localhost:8000/callback?code=")
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
        password: Some("Secret-pass1"),
        user_id: None,
        username: None,
        send_code: true,
        verified: false,
        url_template: "http://studio.chelabase.localhost:8000/verify?code={{.Code}}",
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
        "http://studio.chelabase.localhost:8000/password-reset?userId={{.UserID}}&code={{.Code}}",
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

fn app_user<'a>(id: &'a str, email: &'a str) -> NewUser<'a> {
    NewUser {
        org_id: "p1",
        email,
        given_name: "App",
        family_name: "User",
        password: Some("Secret-pass1"),
        user_id: Some(id),
        username: Some(email),
        send_code: false,
        verified: false,
        url_template: "",
    }
}

#[tokio::test]
async fn new_user_sends_user_id_and_username() {
    let (mock, z) = setup().await;
    mock.add_org("p1");
    let id = z
        .create_user(&app_user("app-1", "ann@example.com"))
        .await
        .unwrap();
    assert_eq!(id, "app-1");
    let u = mock.user_by_id("app-1").unwrap();
    assert_eq!(
        (u.org_id.as_str(), u.username.as_str()),
        ("p1", "ann@example.com")
    );
    assert!(!u.verified);
    // send_code false: no email goes out; true: it does
    assert!(mock.emails().is_empty());
    let mut with_code = app_user("app-2", "bo@example.com");
    with_code.send_code = true;
    with_code.url_template = "http://x/{{.Code}}";
    z.create_user(&with_code).await.unwrap();
    assert_eq!(
        mock.emails(),
        vec![("app-2".to_string(), "verify".to_string())]
    );
    // no password: the user exists but cannot sign in
    let mut none = app_user("app-3", "cy@example.com");
    none.password = None;
    z.create_user(&none).await.unwrap();
    assert!(z.create_session("app-3", "").await.is_err());
}

#[tokio::test]
async fn list_users_pages_with_offsets() {
    let (mock, z) = setup().await;
    for i in 0..5 {
        mock.add_user_in("p1", &format!("u{i}@example.com"), "Secret-pass1", true);
    }
    mock.add_user_in("p2", "other@example.com", "Secret-pass1", true);
    let first = z.list_users("p1", None, 0, 2).await.unwrap();
    assert_eq!(first.users.len(), 2);
    assert_eq!(first.next_offset, Some(2));
    assert!(first.users.iter().all(|u| u.org_id == "p1"));
    let last = z.list_users("p1", None, 4, 2).await.unwrap();
    assert_eq!(last.users.len(), 1);
    assert_eq!(last.next_offset, None);
    let hit = z.list_users("p1", Some("U3@"), 0, 10).await.unwrap();
    assert_eq!(hit.users.len(), 1);
    assert_eq!(hit.users[0].email, "u3@example.com");
    assert!(z.org_has_users("p1").await.unwrap());
    assert!(!z.org_has_users("p9").await.unwrap());
    match z.find_user_in_org("p2", "OTHER@example.com").await.unwrap() {
        EmailMatch::One(found) => assert_eq!(found.org_id, "p2"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        z.find_user_in_org("p1", "other@example.com").await.unwrap(),
        EmailMatch::None
    );
}

#[tokio::test]
async fn ensure_org_is_idempotent() {
    let (mock, _) = setup().await;
    let z = Zitadel::new(&mock.base, ADMIN_TOKEN);
    z.ensure_org("p1", "Project one").await.unwrap();
    z.ensure_org("p1", "Project one").await.unwrap();
    assert_eq!(mock.orgs(), vec!["o1".to_string(), "p1".to_string()]);
}

#[tokio::test]
async fn delete_org_404_is_ok() {
    let (mock, _) = setup().await;
    let z = Zitadel::new(&mock.base, ADMIN_TOKEN);
    z.delete_org("nope").await.unwrap();
    z.ensure_org("p1", "P").await.unwrap();
    z.delete_org("p1").await.unwrap();
    assert_eq!(mock.orgs(), vec!["o1".to_string()]);
}

#[tokio::test]
async fn get_user_reads_state_and_creation_date() {
    let (mock, z) = setup().await;
    let id = mock.add_user_in("p1", "dee@example.com", "Secret-pass1", true);
    let u = z.get_user(&id).await.unwrap().unwrap();
    assert_eq!(
        (u.org_id.as_str(), u.email.as_str()),
        ("p1", "dee@example.com")
    );
    assert!(u.email_verified);
    assert_eq!(u.state, UserState::Active);
    assert!(u.created_at.is_some());
    z.deactivate_user(&id).await.unwrap();
    assert_eq!(
        z.get_user(&id).await.unwrap().unwrap().state,
        UserState::Inactive
    );
    z.reactivate_user(&id).await.unwrap();
    assert_eq!(
        z.get_user(&id).await.unwrap().unwrap().state,
        UserState::Active
    );
    mock.lock(&id);
    assert_eq!(
        z.get_user(&id).await.unwrap().unwrap().state,
        UserState::Locked
    );
    assert!(z.get_user("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn deleting_users_and_sessions_tolerates_404() {
    let (mock, z) = setup().await;
    let id = mock.add_user("eve@example.com", "Secret-pass1", true);
    let session = z.create_session(&id, "Secret-pass1").await.unwrap();
    // the login client holds session.delete
    z.delete_session_admin(&session.id).await.unwrap();
    z.delete_session_admin(&session.id).await.unwrap();
    assert_eq!(mock.session_count(), 0);
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    admin.delete_user(&id).await.unwrap();
    admin.delete_user(&id).await.unwrap();
    assert!(z.get_user(&id).await.unwrap().is_none());
    assert!(mock.calls().contains(&format!("DELETE /v2/users/{id}")));
}

#[tokio::test]
async fn emails_and_passwords_change() {
    let (mock, z) = setup().await;
    let id = mock.add_user("fay@example.com", "Secret-pass1", true);
    z.set_email(&id, "fay2@example.com", false, Some("http://x/{{.Code}}"))
        .await
        .unwrap();
    let u = mock.user("fay2@example.com").unwrap();
    assert!(!u.verified);
    assert_eq!(mock.emails(), vec![(id.clone(), "verify".to_string())]);
    z.set_email(&id, "fay3@example.com", true, None)
        .await
        .unwrap();
    assert!(mock.user("fay3@example.com").unwrap().verified);
    assert_eq!(mock.emails().len(), 1);

    assert!(matches!(
        z.change_password(&id, "wrong", "New-pass-12").await,
        Err(ZError::Rejected { status: 400, .. })
    ));
    z.change_password(&id, "Secret-pass1", "New-pass-12")
        .await
        .unwrap();
    assert_eq!(
        mock.user("fay3@example.com").unwrap().password,
        "New-pass-12"
    );
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    admin.set_password_admin(&id, "Admin-set-1").await.unwrap();
    assert_eq!(
        mock.user("fay3@example.com").unwrap().password,
        "Admin-set-1"
    );
    z.send_password_reset_code(&id, "http://x/{{.Code}}")
        .await
        .unwrap();
    assert!(mock.emails().contains(&(id, "reset".to_string())));
}

#[tokio::test]
async fn admin_calls_need_the_admin_token() {
    let (mock, z) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = mock.add_user("gus@example.com", "Secret-pass1", true);
    // IAM_LOGIN_CLIENT lacks org.create, org.delete and user.delete (v4.19.1 defaults.yaml)
    let forbidden = |r: Result<(), ZError>| matches!(r, Err(ZError::Rejected { status: 403, .. }));
    assert!(forbidden(z.ensure_org("p1", "P").await));
    assert!(forbidden(z.delete_org("o1").await));
    assert!(forbidden(z.delete_user(&id).await));
    assert_eq!(mock.orgs(), vec!["o1".to_string()]);
    assert!(mock.user_by_id(&id).is_some());
    // ... but has user.write, user.credential.write and session.delete
    z.deactivate_user(&id).await.unwrap();
    z.reactivate_user(&id).await.unwrap();
    z.set_password_admin(&id, "Other-pass-1").await.unwrap();
    z.delete_session_admin("s-none").await.unwrap();
    admin.ensure_org("p1", "P").await.unwrap();
    admin.delete_user(&id).await.unwrap();
    admin.delete_org("p1").await.unwrap();
    assert_eq!(mock.orgs(), vec!["o1".to_string()]);
}

#[tokio::test]
async fn users_need_an_existing_org_and_a_unique_username() {
    let (mock, z) = setup().await;
    assert!(matches!(
        z.create_user(&app_user("a1", "x@example.com")).await,
        Err(ZError::Rejected { status: 404, .. })
    ));
    mock.add_org("p1");
    mock.add_org("p2");
    z.create_user(&app_user("a1", "x@example.com"))
        .await
        .unwrap();
    let mut other_org = app_user("a2", "y@example.com");
    other_org.org_id = "p2";
    other_org.username = Some("x@example.com");
    assert!(matches!(
        z.create_user(&other_org).await,
        Err(ZError::Rejected { status: 409, .. })
    ));
}

#[tokio::test]
async fn deleting_a_user_ends_their_sessions() {
    let (mock, z) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = mock.add_user("hal@example.com", "Secret-pass1", true);
    z.create_session(&id, "Secret-pass1").await.unwrap();
    assert_eq!(mock.session_count(), 1);
    admin.delete_user(&id).await.unwrap();
    assert_eq!(mock.session_count(), 0);
}

#[tokio::test]
async fn a_404_that_is_not_json_is_still_a_rejection() {
    let app = axum::Router::new()
        .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "<html>not found</html>") });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let z = Zitadel::new(&base, TOKEN);
    z.delete_user("1").await.unwrap();
    z.delete_org("1").await.unwrap();
    z.delete_session_admin("1").await.unwrap();
    assert_eq!(z.get_user("1").await.unwrap(), None);
    assert!(matches!(
        z.auth_request("V2_1").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

#[tokio::test]
async fn find_user_in_org_reports_ambiguous_emails() {
    let (mock, z) = setup().await;
    assert_eq!(
        z.find_user_in_org("p1", "gus@example.com").await.unwrap(),
        EmailMatch::None
    );
    let first = mock.add_user_in("p1", "gus@example.com", "Secret-pass1", true);
    match z.find_user_in_org("p1", "GUS@example.com").await.unwrap() {
        EmailMatch::One(u) => assert_eq!(u.id, first),
        other => panic!("{other:?}"),
    }
    mock.add_user_in("p1", "Gus@example.com", "Secret-pass1", true);
    assert_eq!(
        z.find_user_in_org("p1", "gus@example.com").await.unwrap(),
        EmailMatch::Many
    );
    // The platform helper keeps answering "no single user".
    assert_eq!(
        z.find_user_by_email("p1", "gus@example.com").await.unwrap(),
        None
    );
}

/// Zitadel v4.19.1 refuses `POST /v2/users/{id}/email` with the address the user already has
/// (`Errors.User.Email.NotChanged`, a precondition); marking the same address verified goes
/// through `PATCH /v2/users/{id}`, which only adds the verified event.
#[tokio::test]
async fn an_unchanged_email_is_refused_and_marked_verified_through_update_user() {
    let (mock, z) = setup().await;
    let id = mock.add_user_in("p1", "hal@example.com", "Secret-pass1", false);
    match z.set_email(&id, "hal@example.com", true, None).await {
        Err(ZError::Rejected { status: 400, message }) => {
            assert!(message.contains("Errors.User.Email.NotChanged"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    z.mark_email_verified(&id, "hal@example.com").await.unwrap();
    let u = z.get_user(&id).await.unwrap().unwrap();
    assert_eq!((u.email.as_str(), u.email_verified), ("hal@example.com", true));
    // Again: nothing to change, and no error.
    z.mark_email_verified(&id, "hal@example.com").await.unwrap();
    assert!(mock.calls().contains(&format!("PATCH /v2/users/{id}")));
    // Unknown users are 404.
    assert!(matches!(
        z.mark_email_verified("missing", "x@example.com").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

/// Members T7: an invitee is a passwordless user whose email goes unverified and unmailed
/// (`returnCode`), so the only mail they get is the invite.
#[tokio::test]
async fn a_user_without_password_gets_no_mail_and_cannot_sign_in() {
    let (mock, _) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = admin
        .create_user_without_password("o1", "new.person@example.com")
        .await
        .unwrap();
    let u = mock.user_by_id(&id).unwrap();
    assert_eq!(
        (u.org_id.as_str(), u.email.as_str(), u.username.as_str(), u.display_name.as_str()),
        ("o1", "new.person@example.com", "new.person@example.com", "new.person")
    );
    assert!(!u.verified);
    assert!(u.password.is_empty());
    assert!(mock.emails().is_empty(), "{:?}", mock.emails());
    assert!(admin.create_session(&id, "").await.is_err());
    // The same email again in the org is a conflict; an unknown org is 404.
    assert!(matches!(
        admin.create_user_without_password("o1", "new.person@example.com").await,
        Err(ZError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        admin.create_user_without_password("nope", "x@example.com").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

/// Zitadel v4.19.1 `POST /v2/users/{userId}/invite_code` with
/// `{"sendCode": {"urlTemplate", "applicationName"}}`: the invite mail carries a fresh code that
/// `invite_code/verify` accepts; a second request replaces it.
#[tokio::test]
async fn an_invite_code_is_mailed_with_the_template_and_verifies() {
    let (mock, _) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = admin
        .create_user_without_password("o1", "ivan@example.com")
        .await
        .unwrap();
    let template = "http://studio/accept-invite?user={{.UserID}}&code={{.Code}}";
    admin.create_invite_code(&id, template).await.unwrap();
    assert_eq!(mock.emails(), vec![(id.clone(), "invite".to_string())]);
    assert_eq!(mock.invite_template(&id).as_deref(), Some(template));
    assert_eq!(mock.invite_application(&id).as_deref(), Some("Chelabase"));
    let first = mock.invite_code(&id).unwrap();
    admin.create_invite_code(&id, template).await.unwrap();
    let second = mock.invite_code(&id).unwrap();
    assert_ne!(first, second);
    assert!(matches!(
        admin.verify_invite_code(&id, &first).await,
        Err(ZError::Rejected { status: 400, .. })
    ));
    admin.verify_invite_code(&id, &second).await.unwrap();
    assert!(mock
        .calls()
        .contains(&format!("POST /v2/users/{id}/invite_code")));
    assert!(matches!(
        admin.create_invite_code("missing", template).await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

/// Zitadel v4.19.1 `POST /v2/users/{userId}/invite_code` with `{"returnCode": {}}`: the code
/// comes back as `inviteCode`, nothing is mailed, and it verifies like a mailed one. A second
/// request replaces it.
#[tokio::test]
async fn a_returned_invite_code_sends_no_mail_and_verifies() {
    let (mock, _) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = admin
        .create_user_without_password("o1", "ivan@example.com")
        .await
        .unwrap();
    let first = admin.return_invite_code(&id).await.unwrap();
    assert!(mock.emails().is_empty());
    assert_eq!(mock.invite_code(&id).as_deref(), Some(first.as_str()));
    assert_eq!(mock.invite_template(&id), None);
    let second = admin.return_invite_code(&id).await.unwrap();
    assert_ne!(first, second);
    assert!(matches!(
        admin.verify_invite_code(&id, &first).await,
        Err(ZError::Rejected { status: 400, .. })
    ));
    admin.verify_invite_code(&id, &second).await.unwrap();
    assert!(matches!(
        admin.return_invite_code("missing").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

/// Zitadel v4.19.1 `GET /v2/users/{userId}/authentication_methods` (no domain query: every
/// method counts, passkeys and U2F of any domain included; a domain query without a domain is a
/// 400, as the live pass found): a user with a password, a passkey, an OTP or an IdP link has
/// methods; one made by an invite has none.
#[tokio::test]
async fn auth_methods_tell_whether_a_user_ever_could_sign_in() {
    let (mock, _) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let fresh = admin.create_user_without_password("o1", "new@example.com").await.unwrap();
    assert!(!admin.has_auth_methods(&fresh).await.unwrap());
    let with_password = mock.add_user("pw@example.com", "Correct-horse-9", true);
    assert!(admin.has_auth_methods(&with_password).await.unwrap());
    for method in ["PASSKEY", "TOTP", "U2F", "OTP_SMS", "OTP_EMAIL", "IDP"] {
        let user = admin.create_user_without_password("o1", &format!("{method}@example.com")).await.unwrap();
        mock.add_auth_method(&user, &format!("AUTHENTICATION_METHOD_TYPE_{method}"));
        assert!(admin.has_auth_methods(&user).await.unwrap(), "{method}");
    }
    assert!(mock
        .calls()
        .iter()
        .any(|c| c.starts_with(&format!("GET /v2/users/{fresh}/authentication_methods"))));
    assert!(matches!(
        admin.has_auth_methods("missing").await,
        Err(ZError::Rejected { status: 404, .. })
    ));
}

/// Zitadel v4.19.1 `POST /admin/v1/email/_search` lists the email providers; one with state
/// `EMAIL_PROVIDER_ACTIVE` can send invites. None, only inactive ones, an HTTP provider and an
/// SMTP provider are told apart.
#[tokio::test]
async fn the_active_email_provider_is_read_from_the_admin_search() {
    let (mock, _) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    assert!(!admin.email_provider_active().await.unwrap());
    mock.set_email_providers(&[("s1", false)]);
    assert!(!admin.email_provider_active().await.unwrap());
    mock.set_email_providers(&[("s1", false), ("s2", true)]);
    assert!(admin.email_provider_active().await.unwrap());
    assert!(mock.calls().contains(&"POST /admin/v1/email/_search".to_string()));
    // The login client's token has no admin rights.
    let login = Zitadel::new(&mock.base, TOKEN);
    assert!(matches!(login.email_provider_active().await, Err(ZError::Rejected { status: 403, .. })));
}

/// `POST /v2/sessions/search` with a `userIdQuery`, then a delete of each: only that user's
/// sessions end.
#[tokio::test]
async fn a_users_sessions_are_found_and_ended() {
    let (mock, z) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let id = mock.add_user("jo@example.com", "Secret-pass1", true);
    let other = mock.add_user("kim@example.com", "Secret-pass1", true);
    z.create_session(&id, "Secret-pass1").await.unwrap();
    z.create_session(&id, "Secret-pass1").await.unwrap();
    z.create_session(&other, "Secret-pass1").await.unwrap();
    assert_eq!(admin.delete_user_sessions(&id).await.unwrap(), 2);
    assert_eq!(mock.session_count(), 1);
    assert_eq!(admin.delete_user_sessions(&id).await.unwrap(), 0);
    assert!(mock.calls().contains(&"POST /v2/sessions/search".to_string()));
}

/// T7 fix M4: the session search is paged; ending a user's sessions searches until none is left.
#[tokio::test]
async fn ending_sessions_pages_until_none_are_left() {
    let (mock, z) = setup().await;
    let admin = Zitadel::new(&mock.base, ADMIN_TOKEN);
    mock.set_session_page(2);
    let id = mock.add_user("lou@example.com", "Secret-pass1", true);
    for _ in 0..5 {
        z.create_session(&id, "Secret-pass1").await.unwrap();
    }
    assert_eq!(admin.delete_user_sessions(&id).await.unwrap(), 5);
    assert_eq!(mock.session_count(), 0);
}

/// T7 fix I2 (R29): whether a user ever set a password (`human.passwordChanged`), and the
/// Initial state, are read; Initial is not Active.
#[tokio::test]
async fn get_user_reads_whether_a_password_is_set() {
    let (mock, z) = setup().await;
    let with = mock.add_user("pat@example.com", "Secret-pass1", true);
    let without = mock.add_user("nop@example.com", "", false);
    assert!(z.get_user(&with).await.unwrap().unwrap().has_password);
    let u = z.get_user(&without).await.unwrap().unwrap();
    assert!(!u.has_password);
    mock.set_initial(&without);
    assert_eq!(z.get_user(&without).await.unwrap().unwrap().state, UserState::Other);
    match z.find_user_in_org("o1", "pat@example.com").await.unwrap() {
        EmailMatch::One(u) => assert!(u.has_password),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn deactivating_twice_is_a_precondition_refusal() {
    let (mock, z) = setup().await;
    let id = mock.add_user_in("p1", "ivy@example.com", "Secret-pass1", true);
    match z.reactivate_user(&id).await {
        Err(ZError::Rejected { status: 400, message }) => {
            assert!(message.contains("Errors.User.NotInactive"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    z.deactivate_user(&id).await.unwrap();
    match z.deactivate_user(&id).await {
        Err(ZError::Rejected { status: 400, message }) => {
            assert!(message.contains("Errors.User.AlreadyInactive"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

// Members hub T8 (D5, S7, S18): `lock_user` and `unlock_user` call Zitadel's
// `POST /v2/users/{id}/lock` and `/unlock`; a user already locked or unlocked, or already gone,
// is fine, so a retry after a failure completes. The mock keeps the state `get_user` reads.
#[tokio::test]
async fn lock_and_unlock_are_idempotent() {
    let (mock, z) = setup().await;
    let id = mock.add_user_in("p1", "lou@example.com", "Secret-pass1", true);
    z.lock_user(&id).await.unwrap();
    assert!(mock.user_by_id(&id).unwrap().locked);
    assert_eq!(z.get_user(&id).await.unwrap().unwrap().state, UserState::Locked);
    z.lock_user(&id).await.unwrap();
    assert_eq!(z.get_user(&id).await.unwrap().unwrap().state, UserState::Locked, "locking twice is fine");
    z.unlock_user(&id).await.unwrap();
    assert!(!mock.user_by_id(&id).unwrap().locked);
    assert_eq!(z.get_user(&id).await.unwrap().unwrap().state, UserState::Active);
    z.unlock_user(&id).await.unwrap();
    assert_eq!(z.get_user(&id).await.unwrap().unwrap().state, UserState::Active, "unlocking twice is fine");
    let calls = mock.calls();
    for verb in ["lock", "unlock"] {
        let want = format!("POST /v2/users/{id}/{verb}");
        assert_eq!(calls.iter().filter(|c| **c == want).count(), 2, "{verb}: {calls:?}");
    }
    z.lock_user("missing").await.unwrap();
    z.unlock_user("missing").await.unwrap();
    // Any other refusal is an error, so the caller can answer 503 and be retried.
    mock.fail("/lock", 500, "Errors.Internal", 1);
    assert!(z.lock_user(&id).await.is_err());
    assert!(!mock.user_by_id(&id).unwrap().locked);
}

// m3 audit N1: "gone" is a definite 404 only. An answer that can't be read as a user is an
// error, never "gone" (it lets a stale fresh-install claim be replaced).
#[tokio::test]
async fn user_is_gone_only_on_not_found() {
    let (mock, z) = setup().await;
    let id = mock.add_user("ada@example.com", "Secret-pass1", true);
    assert!(!z.user_is_gone(&id).await.unwrap());
    mock.remove_user(&id);
    assert!(z.user_is_gone(&id).await.unwrap());
    let other = mock.add_user("bea@example.com", "Secret-pass1", true);
    mock.fail(&format!("GET /v2/users/{other}"), 200, "Odd.Answer", 1);
    assert!(matches!(z.user_is_gone(&other).await, Err(ZError::Unavailable(_))));
    mock.fail(&format!("GET /v2/users/{other}"), 403, "Errors.Denied", 1);
    assert!(matches!(z.user_is_gone(&other).await, Err(ZError::Rejected { status: 403, .. })));
    mock.set_down(true);
    assert!(z.user_is_gone(&other).await.is_err());
}

// #57: `None` is a definite 404 only. A success answer that can't be read as a user (an
// empty body, no `user`, no `user.userId`, malformed JSON) is `Unavailable`, so no caller takes
// it for a deleted user.
#[tokio::test]
async fn get_user_is_none_only_on_not_found() {
    let (mock, z) = setup().await;
    let id = mock.add_user("ada@example.com", "Secret-pass1", true);
    let get = format!("GET /v2/users/{id}");
    for body in [
        "",
        "{}",
        r#"{"user": null}"#,
        r#"{"user": {"state": "USER_STATE_ACTIVE"}}"#,
        r#"{"user": {"userId": 7}}"#,
        r#"{"user": {"userId""#,
    ] {
        mock.answer_raw(&get, 200, body, 1);
        assert!(
            matches!(z.get_user(&id).await, Err(ZError::Unavailable(_))),
            "{body:?}"
        );
        mock.answer_raw(&get, 200, body, 1);
        assert!(
            matches!(z.user_is_gone(&id).await, Err(ZError::Unavailable(_))),
            "{body:?}"
        );
    }
    assert_eq!(z.get_user(&id).await.unwrap().unwrap().id, id);
    mock.remove_user(&id);
    assert_eq!(z.get_user(&id).await.unwrap(), None);
}

// #24: an app reaches Zitadel at its in-network address with the public auth host as `Host`
// (Zitadel picks its instance by Host), as the bootstrap does.
#[tokio::test]
async fn a_client_at_an_in_network_address_names_the_auth_host() {
    use axum::{http::HeaderMap, http::StatusCode, Router};
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = seen.clone();
    let app = Router::new().fallback(move |headers: HeaderMap| {
        let record = record.clone();
        async move {
            let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            record.lock().unwrap().push(host);
            StatusCode::SERVICE_UNAVAILABLE
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let z = Zitadel::new(&format!("http://{addr}"), TOKEN).with_host(Some("auth.example.com"));
    assert!(matches!(z.auth_request("V2_1").await, Err(ZError::Unavailable(_))));
    let plain = Zitadel::new(&format!("http://{addr}"), TOKEN).with_host(None);
    assert!(matches!(plain.auth_request("V2_1").await, Err(ZError::Unavailable(_))));
    assert_eq!(*seen.lock().unwrap(), vec!["auth.example.com".to_string(), addr.to_string()]);
}

// #26: the client never follows a redirect (its calls are REST calls Zitadel answers directly).
// A base URL that redirects to a working Zitadel fails, and the target is never asked.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    use axum::{http::header::LOCATION, http::StatusCode, response::IntoResponse, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let mock = MockZitadel::start().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let target_hits = hits.clone();
    // The redirect's target: counts every request it gets.
    let target = Router::new().fallback(move || {
        target_hits.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::OK }
    });
    let tl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taddr = tl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(tl, target).await.unwrap() });
    let auth_request = mock.add_auth_request(true);
    for (status, to) in [(307u16, format!("http://{taddr}")), (302, mock.base.clone()), (308, mock.base.clone())] {
        let app = Router::new().fallback(move |uri: axum::http::Uri| {
            let to = to.clone();
            async move { (StatusCode::from_u16(status).unwrap(), [(LOCATION, format!("{to}{uri}"))]).into_response() }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        for host in [None, Some("auth.example.com")] {
            let z = Zitadel::new(&format!("http://{addr}"), TOKEN).with_host(host);
            let r = z.auth_request(&auth_request).await;
            assert!(r.is_err(), "{status} to {host:?}: a redirect was followed: {r:?}");
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the redirect's target was asked");
}
