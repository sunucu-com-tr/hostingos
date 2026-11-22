//! End-to-end lifecycle against the in-memory mock panel.

use hos_core::IntegrationError;
use hos_core::integrations::HostingPanel;
use hos_core::integrations::panel::{AccountOwner, AccountRef, AccountSpec, SsoService, SsoTarget};
use hos_core::secret::SecretString;
use hos_panels::mock::MockPanel;
use hos_panels::password::generate_password;
use hos_panels::username::UsernameRules;
use serde_json::json;

fn spec(domain: &str, package: &str) -> AccountSpec {
    AccountSpec {
        domain: domain.into(),
        password: generate_password(hos_panels::MOCK),
        package: package.into(),
        owner: AccountOwner {
            email: "owner@example.com".into(),
            ..AccountOwner::default()
        },
        ..AccountSpec::default()
    }
}

#[tokio::test]
async fn full_lifecycle() {
    let panel = MockPanel::new();
    let pkgs = panel.list_packages().await.unwrap();
    assert!(pkgs.iter().any(|p| p.id == "starter"));
    assert!(pkgs.iter().any(|p| p.reseller));

    let s = spec("Örnek-Site.com.tr", "starter");
    let created = panel.create_account(&s).await.unwrap();
    let user = created.account.username.clone();
    assert!(UsernameRules::CPANEL.validate(&user).is_ok());
    assert_eq!(user, "orneksit");
    assert_eq!(created.account.domain.as_deref(), Some("örnek-site.com.tr"));
    assert!(panel.verify_password(&user, s.password.expose_str()));
    let account = created.account.clone();

    // Same domain again → conflict; another domain with the same base gets a suffix.
    assert!(matches!(
        panel.create_account(&s).await,
        Err(IntegrationError::Conflict(_))
    ));
    let second = panel
        .create_account(&spec("orneksite.net", "starter"))
        .await
        .unwrap();
    assert_eq!(second.account.username, "orneksi1");

    // Suspension blocks SSO.
    panel.suspend(&account, Some("billing")).await.unwrap();
    let info = panel.get_account(&account).await.unwrap();
    assert!(info.suspended);
    assert_eq!(info.suspend_reason.as_deref(), Some("billing"));
    assert!(matches!(
        panel.sso_url(&account, &SsoTarget::default()).await,
        Err(IntegrationError::Conflict(_))
    ));
    panel.unsuspend(&account).await.unwrap();
    let login = panel
        .sso_url(
            &account,
            &SsoTarget {
                app: Some("File Manager".into()),
                ..SsoTarget::default()
            },
        )
        .await
        .unwrap();
    assert!(login.url.expose_str().contains("app=File+Manager"));
    assert!(matches!(
        panel
            .sso_url(
                &account,
                &SsoTarget {
                    service: SsoService::Reseller,
                    ..SsoTarget::default()
                }
            )
            .await,
        Err(IntegrationError::InvalidInput(_))
    ));

    // Package change updates limits; usage stays within them.
    panel.change_package(&account, "business").await.unwrap();
    assert!(matches!(
        panel.change_package(&account, "nope").await,
        Err(IntegrationError::NotFound(_))
    ));
    let usage = panel.usage(&account).await.unwrap();
    assert_eq!(usage.limits.disk_mb, Some(25 * 1024));
    assert!(usage.disk_used_mb.unwrap() <= 25 * 1024);
    let again = panel.usage(&account).await.unwrap();
    assert_eq!(
        usage.disk_used_mb, again.disk_used_mb,
        "usage is deterministic"
    );

    let new_pw = SecretString::from("Another-Passw0rd");
    panel.change_password(&account, &new_pw).await.unwrap();
    assert!(panel.verify_password(&user, "Another-Passw0rd"));

    assert_eq!(panel.list_accounts().await.unwrap().len(), 2);
    panel.terminate(&account).await.unwrap();
    assert!(matches!(
        panel.terminate(&account).await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        panel.usage(&account).await,
        Err(IntegrationError::NotFound(_))
    ));
}

#[tokio::test]
async fn validation_errors() {
    let panel = MockPanel::new();
    assert!(matches!(
        panel.create_account(&spec("no-dot", "starter")).await,
        Err(IntegrationError::InvalidInput(_))
    ));
    assert!(matches!(
        panel.create_account(&spec("x.com", "missing")).await,
        Err(IntegrationError::NotFound(_))
    ));
    let mut bad_user = spec("x.com", "starter");
    bad_user.username = "root".into();
    assert!(matches!(
        panel.create_account(&bad_user).await,
        Err(IntegrationError::InvalidInput(_))
    ));
    assert!(matches!(
        panel.suspend(&AccountRef::new("ghost"), None).await,
        Err(IntegrationError::NotFound(_))
    ));
}

#[tokio::test]
async fn configured_failures_and_shared_namespace() {
    let config = json!({"namespace": "it-shared", "fail_operations": "suspend"});
    let a = hos_panels::build(hos_panels::MOCK, &config, &json!({})).unwrap();
    let created = a
        .create_account(&spec("shared.test", "starter"))
        .await
        .unwrap();
    let err = a.suspend(&created.account, None).await.unwrap_err();
    assert!(err.is_retryable());
    assert_eq!(err.kind(), "remote");

    // A rebuilt instance with the same namespace sees the account.
    let b = hos_panels::build(
        hos_panels::MOCK,
        &json!({"namespace": "it-shared", "packages": "[{\"id\":\"tiny\",\"name\":\"Tiny\"}]"}),
        &json!({}),
    )
    .unwrap();
    assert!(b.get_account(&created.account).await.is_ok());
    assert_eq!(b.list_packages().await.unwrap()[0].id, "tiny");
    b.suspend(&created.account, None).await.unwrap();

    let failing = MockPanel::new().failing("create_account");
    assert!(
        failing
            .create_account(&spec("f.test", "starter"))
            .await
            .is_err()
    );
}

/// The packages the demo seed stores as a JSON array (not a string) are read,
/// and an account on one of them is created.
#[tokio::test]
async fn packages_from_a_json_array_config() {
    let panel = MockPanel::from_config(&json!({
        "namespace": "packages-array-config",
        "packages": [
            {"id": "pro", "name": "Pro", "limits": {"disk_mb": 25600, "domains": 5}},
            {"id": "starter", "name": "Starter", "limits": {"disk_mb": 5120}}
        ]
    }))
    .expect("array config");
    let pkgs = panel.list_packages().await.unwrap();
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].limits.disk_mb, Some(25600));
    panel
        .create_account(&spec("dizi.example.com", "pro"))
        .await
        .expect("account on an array-configured package");
    // A string holding the same JSON works too; anything else is refused.
    assert!(
        MockPanel::from_config(&json!({
            "namespace": "packages-string-config",
            "packages": "[{\"id\":\"x\",\"name\":\"X\"}]"
        }))
        .is_ok()
    );
    assert!(MockPanel::from_config(&json!({"packages": 5})).is_err());
    // The registry path validates the descriptor first; an array must pass
    // it too (it used to be refused as "config.packages has an invalid value").
    let built = hos_panels::build(
        hos_panels::MOCK,
        &json!({"namespace": "packages-array-build", "packages": [{"id": "pro", "name": "Pro"}]}),
        &json!({}),
    )
    .expect("descriptor accepts an array package list");
    assert_eq!(built.list_packages().await.unwrap().len(), 1);
}
