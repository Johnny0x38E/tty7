//! Settings → Mobile: switching phone access on, pairing a phone, and the
//! phones already paired.
//!
//! The gateway itself is run by the local daemon (`tty7_core::daemon::mobile`),
//! which reads `mobile_access` from the config. Everything this page shows or changes about
//! it goes through the gateway's state directory — the files are the interface
//! between this process and that one, as they are for `tty7-gateway` on the
//! command line.

use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tty7_gateway::state::{State, Status};

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

/// How long a pairing code on screen stays good.
const PAIR_TTL: Duration = Duration::from_secs(600);
/// How often an open pairing looks for the phone that used it.
const PAIR_POLL: Duration = Duration::from_secs(1);
const QR_SIZE: f32 = 208.;
/// How long switching phone access on may take before it counts as failed:
/// the daemon notices within a couple of seconds, and a gateway binds in one.
const START_TIMEOUT: Duration = Duration::from_secs(15);
const START_POLL: Duration = Duration::from_millis(500);

/// A pairing code on screen.
pub(crate) struct Pairing {
    code: String,
    qr: Option<Arc<gpui::Image>>,
    /// The phones paired before this code was made, to tell the new one by.
    before: Vec<String>,
    until: Instant,
}

/// What the switch shows: whether phones can actually reach this machine,
/// not merely whether they were asked to be allowed.
enum Access {
    Off,
    Starting,
    On,
    /// Asked for, and the daemon could not start it: shown off, with why.
    Failed(String),
}

/// The gateway's state directory, when there is one to look at. Opening it
/// creates it, which is only worth doing once phone access is on.
fn gateway_state(on: bool) -> Option<State> {
    let dir = crate::core::config::config_dir_path()?.join("mobile");
    (on || dir.is_dir()).then(State::open_default)?.ok()
}

fn access(on: bool, starting: bool, state: Option<&State>) -> Access {
    let serving = state.is_some_and(State::serving);
    match (on, serving, starting) {
        (false, _, _) => Access::Off,
        (true, true, _) => Access::On,
        (true, false, true) => Access::Starting,
        (true, false, false) => match state.and_then(State::status) {
            Some(Status::Failed { error }) => Access::Failed(error),
            // The daemon has not got to it yet, or is between retries.
            _ => Access::Starting,
        },
    }
}

/// The code as a QR image: dark modules on white with a quiet zone, whatever
/// the theme, since that is what a phone's camera reads.
fn qr_image(code: &str) -> Option<Arc<gpui::Image>> {
    let qr =
        qrcode::QrCode::with_error_correction_level(code.as_bytes(), qrcode::EcLevel::L).ok()?;
    let width = qr.width();
    let (quiet, scale) = (2, 8);
    let side = ((width + 2 * quiet) * scale) as u32;
    let mut img = image::GrayImage::from_pixel(side, side, image::Luma([255]));
    for (i, color) in qr.to_colors().iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (x0, y0) = ((i % width + quiet) * scale, (i / width + quiet) * scale);
        for y in y0..y0 + scale {
            for x in x0..x0 + scale {
                img.put_pixel(x as u32, y as u32, image::Luma([0]));
            }
        }
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png,
    )))
}

impl Tty7App {
    /// Switching phone access on is a request to the daemon, so the switch
    /// holds at "starting" until a gateway is actually serving. If none comes
    /// up, the switch goes back off and says why, rather than stay on over
    /// a gateway that is not there.
    pub(crate) fn set_mobile_access(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !on {
            self.update_config(cx, |cfg| cfg.mobile_access = false);
            if let Some(s) = self.active_settings_mut() {
                s.mobile_pairing = None;
                s.mobile_starting = false;
            }
            cx.notify();
            return;
        }
        // A failure from an earlier attempt must not be mistaken for this
        // one's. Nothing is serving, so the file is nobody's to keep.
        if let Some(state) = gateway_state(true)
            && !state.serving()
        {
            let _ = state.set_status(&Status::Stopped);
        }
        self.update_config(cx, |cfg| cfg.mobile_access = true);
        if let Some(s) = self.active_settings_mut() {
            s.mobile_starting = true;
        }
        cx.notify();

        let deadline = Instant::now() + START_TIMEOUT;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                smol::Timer::after(START_POLL).await;
                let state = gateway_state(true);
                let failure = match state.as_ref() {
                    Some(state) if state.serving() => None,
                    Some(state) => match state.status() {
                        Some(Status::Failed { error }) => Some(error),
                        _ if Instant::now() >= deadline => {
                            Some(t(L10nKey::SettingsMobileNoAnswer).to_string())
                        }
                        _ => continue,
                    },
                    None => Some(t(L10nKey::SettingsMobileNoAnswer).to_string()),
                };
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Some(s) = this.active_settings_mut() {
                        s.mobile_starting = false;
                    }
                    if let Some(error) = failure {
                        this.update_config(cx, |cfg| cfg.mobile_access = false);
                        window.push_notification(
                            t_fmt(L10nKey::SettingsMobileStartFailed, &[("error", &error)]),
                            cx,
                        );
                    }
                    cx.notify();
                });
                return;
            }
        })
        .detach();
    }

    fn start_mobile_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(state) = gateway_state(true) else {
            return;
        };
        let code = match tty7_gateway::service::pair_code(&state, PAIR_TTL) {
            Ok(code) => code,
            Err(e) => {
                log::warn!("could not make a pairing code: {e:#}");
                return;
            }
        };
        let before = state
            .devices()
            .unwrap_or_default()
            .into_iter()
            .map(|d| d.id)
            .collect();
        if let Some(s) = self.active_settings_mut() {
            s.mobile_pairing = Some(Pairing {
                qr: qr_image(&code),
                code,
                before,
                until: Instant::now() + PAIR_TTL,
            });
            s.mobile_paired = None;
            s.mobile_copied = false;
        }
        cx.notify();

        // Watch for the phone that uses it; the code closes itself then, or
        // when it runs out.
        cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(PAIR_POLL).await;
                let alive = this.update(cx, |this, cx| {
                    let Some(s) = this.active_settings_mut() else {
                        return false;
                    };
                    let Some(pairing) = &s.mobile_pairing else {
                        return false;
                    };
                    let new = state
                        .devices()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|d| !pairing.before.contains(&d.id));
                    let expired = Instant::now() >= pairing.until;
                    if let Some(device) = new {
                        s.mobile_paired = Some(device.name);
                    }
                    let done = s.mobile_paired.is_some() || expired;
                    if done {
                        s.mobile_pairing = None;
                    }
                    cx.notify();
                    !done
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    fn unpair_mobile_device(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(state) = gateway_state(true)
            && let Err(e) = state.revoke(&id)
        {
            log::warn!("could not unpair {id}: {e:#}");
        }
        cx.notify();
    }

    pub(crate) fn render_settings_mobile(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let on = cx.global::<Config>().mobile_access;
        let state = gateway_state(on);
        let starting = self.active_settings().is_some_and(|s| s.mobile_starting);
        let access = access(on, starting, state.as_ref());
        let serving = state.as_ref().is_some_and(State::serving);

        let switch = kit::switch("mobile-access")
            .checked(matches!(access, Access::On | Access::Starting))
            .disabled(matches!(access, Access::Starting))
            .on_click(
                cx.listener(|this, on: &bool, window, cx| this.set_mobile_access(*on, window, cx)),
            )
            .into_any_element();
        let access_desc = match &access {
            Access::Off | Access::On => t(L10nKey::SettingsMobileAccessDesc).to_string(),
            Access::Starting => t(L10nKey::SettingsMobileStatusStarting).to_string(),
            Access::Failed(error) => {
                t_fmt(L10nKey::SettingsMobileStatusFailed, &[("error", error)])
            }
        };

        let pairing = self
            .active_settings()
            .and_then(|s| s.mobile_pairing.as_ref());
        let paired = self.active_settings().and_then(|s| s.mobile_paired.clone());
        let pair_button = self
            .settings_button(
                "mobile-pair",
                t(L10nKey::SettingsMobileShowCode),
                cx,
                |this, _, cx| this.start_mobile_pairing(cx),
            )
            .disabled(!serving || pairing.is_some())
            .into_any_element();
        let pair_desc = match (serving, &paired) {
            (_, Some(name)) => t_fmt(L10nKey::SettingsMobilePaired, &[("name", name)]),
            (true, None) => t(L10nKey::SettingsMobilePairDesc).to_string(),
            (false, None) => t(L10nKey::SettingsMobilePairNeedsAccess).to_string(),
        };

        let access_group = self.settings_group(
            None,
            None,
            [self
                .settings_row(t(L10nKey::SettingsMobileAccess), access_desc, switch, cx)
                .into_any_element()],
            cx,
        );

        let mut pair_rows = vec![
            self.settings_row(t(L10nKey::SettingsMobilePair), pair_desc, pair_button, cx)
                .into_any_element(),
        ];
        if let Some(pairing) = pairing {
            pair_rows.push(self.render_mobile_pairing(pairing, &tk, cx));
        }
        let pair_group = self.settings_group(None, None, pair_rows, cx);

        let devices = state
            .as_ref()
            .and_then(|s| s.devices().ok())
            .unwrap_or_default();
        let phone_rows: Vec<AnyElement> = if devices.is_empty() {
            vec![
                div()
                    .py(px(10.))
                    .text_size(fs(13.))
                    .text_color(tk.k5)
                    .child(t(L10nKey::SettingsMobileNoPhones))
                    .into_any_element(),
            ]
        } else {
            devices
                .into_iter()
                .map(|device| {
                    let id = device.id.clone();
                    let unpair =
                        kit::button(
                            SharedString::from(format!("mobile-unpair-{}", device.id)),
                            t(L10nKey::SettingsMobileUnpair),
                            BtnKind::Danger,
                        )
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            this.unpair_mobile_device(id.clone(), cx)
                        }))
                        .into_any_element();
                    self.settings_row(
                        device.name,
                        device.id[..device.id.len().min(12)].to_string(),
                        unpair,
                        cx,
                    )
                    .into_any_element()
                })
                .collect()
        };
        let phones_group =
            self.settings_group(Some(t(L10nKey::SettingsMobilePhones)), None, phone_rows, cx);

        Self::settings_page([access_group, pair_group, phones_group])
    }

    fn render_mobile_pairing(
        &self,
        pairing: &Pairing,
        tk: &Tk,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.active_settings().is_some_and(|s| s.mobile_copied);
        let code = pairing.code.clone();
        let copy = kit::button(
            "mobile-copy-code",
            if copied {
                t(L10nKey::SettingsCopied)
            } else {
                t(L10nKey::SettingsMobileCopyCode)
            },
            BtnKind::Secondary,
        )
        .on_click(cx.listener(move |this, _, _w, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
            if let Some(s) = this.active_settings_mut() {
                s.mobile_copied = true;
            }
            cx.notify();
            cx.spawn(async move |this, cx| {
                smol::Timer::after(Duration::from_millis(1500)).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(s) = this.active_settings_mut() {
                        s.mobile_copied = false;
                        cx.notify();
                    }
                });
            })
            .detach();
        }))
        .into_any_element();
        let cancel = kit::button("mobile-pair-cancel", t(L10nKey::Cancel), BtnKind::Link)
            .on_click(cx.listener(|this, _, _w, cx| {
                if let Some(s) = this.active_settings_mut() {
                    s.mobile_pairing = None;
                }
                cx.notify();
            }))
            .into_any_element();

        let qr = match &pairing.qr {
            Some(image) => gpui::img(image.clone())
                .size(px(QR_SIZE))
                .into_any_element(),
            None => div().size(px(QR_SIZE)).into_any_element(),
        };
        h_flex()
            .id("mobile-pairing")
            .mt(px(12.))
            .gap(px(24.))
            .items_start()
            // White whatever the theme: a camera reads dark on light.
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .rounded(px(12.))
                    .bg(gpui::white())
                    .border_1()
                    .border_color(tk.k08)
                    .child(qr),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(fs(13.))
                            .text_color(tk.fg)
                            .child(t(L10nKey::SettingsMobilePairScan)),
                    )
                    .child(
                        div()
                            .p(px(8.))
                            .rounded(px(6.))
                            .bg(tk.k04)
                            .text_size(fs(11.))
                            .font_family(Tk::mono(cx))
                            .text_color(tk.k6)
                            .line_clamp(3)
                            .text_ellipsis()
                            .child(pairing.code.clone()),
                    )
                    .child(
                        h_flex()
                            .gap(px(12.))
                            .items_center()
                            .child(copy)
                            .child(cancel),
                    )
                    .child(
                        div()
                            .text_size(fs(12.))
                            .text_color(tk.k5)
                            .child(t(L10nKey::SettingsMobilePairValid)),
                    ),
            )
            .into_any_element()
    }
}
