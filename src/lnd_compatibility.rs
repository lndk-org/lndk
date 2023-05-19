mod internal {
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

// On startup error out if the following checks don't pass:
// - That GetVersion has peersrpc, signerrpc and dev build tags.
// - That the version is > v0.16.2

pub struct Compatibility;

pub struct VersionInfo {
    pub version: String,

    pub app_minor: u32,

    pub app_patch: u32,

    pub build_tags: Vec<String>,
}

impl Compatibility {
    pub async fn check_compatibility(version_info: VersionInfo) -> Result<i32, String> {
        let version: &str = &version_info.version;

        let tags = &version_info.build_tags;

        if version_info.app_minor < 16 || version_info.app_patch < 2 {
            Err(String::from(format!(
                "LND version {} is less than the version required to run LNDK",
                version
            )))
        } else if !tags.contains(&String::from("peersrpc")) {
            Err(String::from(
                "Please make sure the 'peersrpc' service is enabled when compiling LND",
            ))
        } else if !tags.contains(&String::from("signerrpc")) {
            Err(String::from(
                "Please make sure the 'signerrpc' service is enabled when compiling LND",
            ))
        } else if !tags.contains(&String::from("dev")) {
            Err(String::from(
                "Please make sure the 'dev' service is enabled when compiling LND",
            ))
        } else {
            Ok(0)
        }
    }
}

// we define multiple test cases using different version and build tag configurations.
// We call the check_compatibility method with the respective VersionInfo object for each
// test case and assert the expected results using the assert macros provided by the Rust testing framework.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_compatibility() {
        // Test case 1: LND version is less than required

        let version_info = VersionInfo {
            version: String::from("0.15.0"),
            app_minor: 15,
            app_patch: 0,
            build_tags: vec![
                String::from("peersrpc"),
                String::from("signerrpc"),
                String::from("dev"),
            ],
        };
        let result = Compatibility::check_compatibility(version_info).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "LND version 0.15.0 is less than the version required to run LNDK"
        );
    }
    // Test case 2: 'peersrpc' service is not enabled

    #[tokio::test]
    async fn test_check_peersrpc_service() {
        let version_info = VersionInfo {
            version: String::from("0.16.2"),
            app_minor: 16,
            app_patch: 2,
            build_tags: vec![String::from("signerrpc"), String::from("dev")],
        };
        let result = Compatibility::check_compatibility(version_info).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Please make sure the 'peersrpc' service is enabled when compiling LND"
        );
    }

    // Test case 3: 'signerrpc' service is not enabled
    #[tokio::test]
    async fn test_check_signerrpc_service() {
        let version_info = VersionInfo {
            version: String::from("0.16.2"),
            app_minor: 16,
            app_patch: 2,
            build_tags: vec![String::from("peersrpc"), String::from("dev")],
        };
        let result = Compatibility::check_compatibility(version_info).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Please make sure the 'signerrpc' service is enabled when compiling LND"
        );
    }

    // Test case 4: 'dev' service is not enabled
    #[tokio::test]
    async fn test_check_dev_service() {
        let version_info = VersionInfo {
            version: String::from("0.16.2"),
            app_minor: 16,
            app_patch: 2,
            build_tags: vec![String::from("peersrpc"), String::from("signerrpc")],
        };
        let result = Compatibility::check_compatibility(version_info).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Please make sure the 'dev' service is enabled when compiling LND"
        );
    }

    // Test case 5: All conditions met
    #[tokio::test]
    async fn test_check_all_conditions() {
        let version_info = VersionInfo {
            version: String::from("0.16.2"),
            app_minor: 16,
            app_patch: 2,
            build_tags: vec![
                String::from("peersrpc"),
                String::from("signerrpc"),
                String::from("dev"),
            ],
        };
        let result = Compatibility::check_compatibility(version_info).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }
}
