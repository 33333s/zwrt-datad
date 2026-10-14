//! The OEM power state. With "quick power-on" (`zwrt_zte_mc.power.quicken_power_on`)
//! a power-off only pretends: the stock modules drop mobile data, Wi-Fi and the
//! modems and switch the screen off, but Linux keeps running, and while
//! charging the `usbIsCharging` wake lock keeps it from suspending. datad then
//! goes dormant: no sampling, cloud session or background jobs until power-on.
use std::sync::LazyLock;
use tokio::sync::watch;

static DORMANT: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::Sender::new(false));

/// Whether the OEM power manager is in a power-off state
/// (`zwrt_zte_mc_tmp.mode.mode_main_state` is `mode_power_off*`). A plain file
/// read, cheap enough to poll.
pub fn oem_powered_off() -> bool {
    std::fs::read_to_string(
        std::env::var("ZWRT_DATAD_MC_STATE_PATH")
            .unwrap_or_else(|_| "/etc/config/zwrt_zte_mc_tmp".into()),
    )
    .is_ok_and(|raw| power_off_state(&raw))
}

fn power_off_state(raw: &str) -> bool {
    raw.lines().any(|line| {
        line.trim()
            .strip_prefix("option mode_main_state")
            .is_some_and(|value| {
                value
                    .trim()
                    .trim_matches(['\'', '"'])
                    .starts_with("mode_power_off")
            })
    })
}

pub fn dormant() -> bool {
    *DORMANT.borrow()
}

pub fn subscribe() -> watch::Receiver<bool> {
    DORMANT.subscribe()
}

/// Records the state; true when it changed.
pub fn set_dormant(value: bool) -> bool {
    DORMANT.send_if_modified(|current| {
        let changed = *current != value;
        *current = value;
        changed
    })
}

/// Returns at once while the device is on, otherwise at power-on.
pub async fn awake() {
    let _ = subscribe().wait_for(|dormant| !*dormant).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oem_power_off_states_are_recognised() {
        let state = |mode: &str| {
            format!(
                "\nconfig zudata_mode_fun 'mode'\n\toption reboot_reason_code '1182'\n\toption mode_main_state '{mode}'\n"
            )
        };
        for mode in [
            "mode_power_off_quicken",
            "mode_power_off_charger_quicken",
            "mode_power_off_quickening",
            "mode_power_off",
        ] {
            assert!(power_off_state(&state(mode)), "{mode}");
        }
        for mode in ["mode_power_on", "mode_power_on_charger", "mode_initial"] {
            assert!(!power_off_state(&state(mode)), "{mode}");
        }
        assert!(!power_off_state(""));
        assert!(!power_off_state("\toption other 'mode_power_off'\n"));
    }

    #[tokio::test]
    async fn awake_waits_for_power_on() {
        assert!(set_dormant(true));
        assert!(!set_dormant(true));
        assert!(dormant());
        let waiter = tokio::spawn(awake());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        assert!(set_dormant(false));
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        awake().await;
    }
}
