//! iOS backend via the `idevice` crate.
//!
//! The crate is async (tokio). We wrap it behind a sync façade
//! so [`crate::DeviceManager::list`] can be called from any
//! thread without callers having to drag a runtime in. The
//! backend owns a small multi-threaded tokio runtime
//! (`worker_threads = 1`) created in `new()`; every public
//! method funnels through `runtime.block_on(...)`.
//!
//! Why we own a runtime instead of using `block_on` on an
//! ambient one: the GUI thread is gpui's main runloop, not a
//! tokio reactor. We can't park it. The dedicated runtime
//! sidesteps the cross-runtime-block panic that `tokio::runtime
//! ::Handle::block_on` raises when called from a non-tokio
//! thread.

use std::sync::Arc;

use tokio::runtime::Runtime;

use crate::{AuthState, DeviceError, DeviceId, DeviceInfo, Transport};

pub struct IDeviceBackend {
    /// Owned tokio runtime, one thread, used for every
    /// idevice/lockdown call. `Arc`d so the backend can be
    /// cloned cheaply if we ever need to. Constructed once at
    /// backend init; lives the whole app lifetime.
    rt: Arc<Runtime>,
    /// `true` if `UsbmuxdConnection::default().await` succeeded
    /// during `new()`. We don't keep the connection open —
    /// usbmuxd connections are stateful and per-request — but
    /// the initial probe tells us whether the daemon is
    /// reachable on this machine at all.
    usbmuxd_reachable: bool,
}

impl IDeviceBackend {
    pub fn new() -> Result<Self, DeviceError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("glass-device-ios")
            .build()
            .map_err(|e| {
                DeviceError::IosBackendUnavailable(format!(
                    "tokio runtime init failed: {e}"
                ))
            })?;
        let usbmuxd_reachable = rt.block_on(probe_usbmuxd());
        Ok(Self {
            rt: Arc::new(rt),
            usbmuxd_reachable,
        })
    }

    pub fn usbmuxd_reachable(&self) -> bool {
        self.usbmuxd_reachable
    }

    /// Snapshot every USB-attached iOS device. Each entry's
    /// lockdownd properties are fetched on-demand; failures to
    /// reach lockdownd (most often, the device isn't paired
    /// yet) downgrade the entry to `AuthState::Unauthorised`
    /// rather than failing the whole list call — we want
    /// "iPhone plugged in but waiting on Trust" to render as a
    /// useful row.
    pub fn list(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        if !self.usbmuxd_reachable {
            return Ok(Vec::new());
        }
        self.rt
            .block_on(list_inner())
            .map_err(|e| DeviceError::Backend(e.to_string()))
    }
}

async fn probe_usbmuxd() -> bool {
    use idevice::usbmuxd::UsbmuxdConnection;
    UsbmuxdConnection::default().await.is_ok()
}

async fn list_inner() -> anyhow::Result<Vec<DeviceInfo>> {
    use idevice::lockdown::LockdownClient;
    use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection};
    use idevice::IdeviceService;
    let mut conn = UsbmuxdConnection::default().await?;
    let devs = conn.get_devices().await?;
    // Resolve once; passed to every per-device provider below.
    let addr = UsbmuxdAddr::from_env_var()
        .map_err(|e| anyhow::anyhow!("usbmuxd addr: {e}"))?;
    let mut out = Vec::with_capacity(devs.len());
    for d in devs {
        let mut info = DeviceInfo {
            id: DeviceId::ios(&d.udid),
            model: None,
            os_version: None,
            transport: Transport::Usb,
            state: AuthState::Unauthorised,
        };
        // Try to open a lockdown session and read DeviceName /
        // ProductType / ProductVersion. Failure means the
        // device isn't paired (or the user hasn't tapped Trust
        // yet) — surface that as Unauthorised.
        let provider = d.to_provider(addr.clone(), "glass-device");
        match LockdownClient::connect(&provider).await {
            Ok(mut lock) => {
                info.state = AuthState::Authorised;
                info.model = read_lockdown_string(&mut lock, "DeviceClass").await
                    .or(read_lockdown_string(&mut lock, "ProductType").await);
                // Prefer the user-set DeviceName ("Andrew's
                // iPhone") for the chip label. Fall back to
                // ProductType ("iPhone15,3") which is at least
                // self-explanatory.
                if let Some(name) =
                    read_lockdown_string(&mut lock, "DeviceName").await
                {
                    info.model = Some(name);
                }
                info.os_version =
                    read_lockdown_string(&mut lock, "ProductVersion").await;
            }
            Err(e) => {
                tracing::debug!(udid = %d.udid, ?e, "glass-device: lockdown handshake failed; treating as Unauthorised");
            }
        }
        out.push(info);
    }
    Ok(out)
}

/// Read a single string-valued property from a lockdown client.
/// Returns `None` for any error or non-string value — callers
/// treat absence as "unknown" rather than fatal.
async fn read_lockdown_string(
    lock: &mut idevice::lockdown::LockdownClient,
    key: &str,
) -> Option<String> {
    let value = lock.get_value(Some(key), None).await.ok()?;
    plist_to_string(&value)
}

fn plist_to_string(v: &plist::Value) -> Option<String> {
    match v {
        plist::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}
