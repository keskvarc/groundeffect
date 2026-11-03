use crate::{
    geo::Footprint,
    model::{FlightPhase, TelemetryState},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub const DEFAULT_XPLANE_UDP: &str = "127.0.0.1:49000";
pub const STALE_AFTER: Duration = Duration::from_secs(5);
const SUBSCRIBE_INTERVAL: Duration = Duration::from_secs(2);
const RPOS_PACKET_LEN: usize = 69;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlightSample {
    pub lat: f64,
    pub lon: f64,
    pub elevation_msl_m: f64,
    pub agl_m: f32,
    pub pitch_deg: f32,
    pub heading_deg: f32,
    pub roll_deg: f32,
    pub vx_east_mps: f32,
    pub vy_up_mps: f32,
    pub vz_south_mps: f32,
    pub ground_speed_mps: f32,
    pub track_deg: f32,
    pub turn_rate_deg_s: f32,
    pub phase: FlightPhase,
    pub received_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TelemetrySnapshot {
    pub state: TelemetryState,
    pub phase: FlightPhase,
    pub sample: Option<FlightSample>,
    pub packets: u64,
    pub invalid_packets: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReferenceKinematics {
    pub lat: f64,
    pub lon: f64,
    pub agl_m: f32,
    pub ground_speed_mps: f32,
    pub track_deg: f32,
    pub turn_rate_deg_s: f32,
    pub phase: FlightPhase,
    pub from_live_telemetry: bool,
}

impl ReferenceKinematics {
    pub fn from_sample(sample: &FlightSample) -> Self {
        Self {
            lat: sample.lat,
            lon: sample.lon,
            agl_m: sample.agl_m,
            ground_speed_mps: sample.ground_speed_mps,
            track_deg: sample.track_deg,
            turn_rate_deg_s: sample.turn_rate_deg_s,
            phase: sample.phase,
            from_live_telemetry: true,
        }
    }

    pub fn fallback(lat: f64, lon: f64) -> Self {
        Self {
            lat,
            lon,
            agl_m: 0.0,
            ground_speed_mps: 0.0,
            track_deg: 0.0,
            turn_rate_deg_s: 0.0,
            phase: FlightPhase::Parked,
            from_live_telemetry: false,
        }
    }

    pub fn footprint_metrics(&self, footprint: Footprint) -> (f64, f64) {
        let b = footprint.geographic();
        let closest_lat = self.lat.clamp(b.south, b.north);
        let closest_lon = self.lon.clamp(b.west, b.east);
        let cos_lat = self.lat.to_radians().cos().abs().max(0.01);
        let dy_north = (closest_lat - self.lat) * 111_132.0;
        let dx_east = (closest_lon - self.lon) * 111_320.0 * cos_lat;
        let distance_m = dx_east.hypot(dy_north);
        if distance_m < 1.0 {
            return (0.0, 0.0);
        }
        let center_lat = (b.south + b.north) * 0.5;
        let center_lon = (b.west + b.east) * 0.5;
        let cy_north = (center_lat - self.lat) * 111_132.0;
        let cx_east = (center_lon - self.lon) * 111_320.0 * cos_lat;
        let bearing_deg = cx_east.atan2(cy_north).to_degrees().rem_euclid(360.0);
        let predicted_track =
            (self.track_deg as f64 + self.turn_rate_deg_s as f64 * 5.0).rem_euclid(360.0);
        let diff_now = angular_diff_deg(bearing_deg, self.track_deg as f64);
        let diff_turn = angular_diff_deg(bearing_deg, predicted_track);
        (distance_m, diff_now.min(diff_turn))
    }

    pub fn guard_radius_m(&self) -> f64 {
        match self.phase {
            FlightPhase::Unknown | FlightPhase::Parked | FlightPhase::Taxi => 18_000.0,
            FlightPhase::Climb | FlightPhase::Descent => {
                (18_000.0 + self.agl_m.max(0.0) as f64 * 4.0).clamp(18_000.0, 35_000.0)
            }
            FlightPhase::Cruise => {
                (25_000.0 + self.agl_m.max(0.0) as f64 * 3.0).clamp(25_000.0, 45_000.0)
            }
        }
    }

    pub fn corridor_reach_m(&self, conservative_cost_s: f64) -> f64 {
        let horizon_s = (180.0 + conservative_cost_s * 60.0).clamp(180.0, 420.0);
        let effective_speed_mps = match self.phase {
            FlightPhase::Unknown | FlightPhase::Parked => 40.0,
            FlightPhase::Taxi => (self.ground_speed_mps as f64).max(55.0),
            FlightPhase::Climb | FlightPhase::Cruise | FlightPhase::Descent => {
                (self.ground_speed_mps as f64).max(65.0)
            }
        };
        (self.guard_radius_m() + effective_speed_mps * horizon_s).clamp(28_000.0, 90_000.0)
    }

    pub fn corridor_half_angle_deg(&self) -> f64 {
        if !self.from_live_telemetry {
            return 180.0;
        }
        let base = match self.phase {
            FlightPhase::Unknown | FlightPhase::Parked => 90.0,
            FlightPhase::Taxi => 75.0,
            FlightPhase::Climb | FlightPhase::Descent => 55.0,
            FlightPhase::Cruise => 45.0,
        };
        (base + (self.turn_rate_deg_s.abs() as f64) * 12.0).min(140.0)
    }

    pub fn is_protected(&self, footprint: Footprint) -> bool {
        let (distance_m, _) = self.footprint_metrics(footprint);
        distance_m <= self.guard_radius_m()
    }

    pub fn is_behind(&self, footprint: Footprint) -> bool {
        if !self.from_live_telemetry || self.ground_speed_mps < 5.0 {
            return false;
        }
        let (distance_m, angle_deg) = self.footprint_metrics(footprint);
        distance_m > self.guard_radius_m() && angle_deg > 110.0
    }

    pub fn deadline_slack_millis(&self, footprint: Footprint) -> i64 {
        let conservative_cost_s = 1.0;
        let (distance_m, angle_deg) = self.footprint_metrics(footprint);
        let speed_mps = (self.ground_speed_mps as f64).max(35.0);
        let directional_penalty = if self.from_live_telemetry {
            1.0 + (angle_deg / 180.0) * 0.75
        } else {
            1.0
        };
        let time_until_needed_s = (distance_m * directional_penalty) / speed_mps;
        let zoom_bonus_s = (footprint.zoom.saturating_sub(16) as f64) * 120.0;
        let slack_s = time_until_needed_s - conservative_cost_s - zoom_bonus_s;
        (slack_s * 1000.0)
            .round()
            .clamp(-1_000_000_000.0, 1_000_000_000.0) as i64
    }

    pub fn classify_lookahead(&self, footprint: Footprint) -> Option<u8> {
        let conservative_cost_s = 1.0;
        let (distance_m, angle_deg) = self.footprint_metrics(footprint);
        if distance_m <= 8_500.0 || (footprint.zoom >= 17 && distance_m <= 16_000.0) {
            return Some(1);
        }
        let guard_m = self.guard_radius_m();
        if distance_m <= guard_m {
            return Some(2);
        }
        let corridor_m = self.corridor_reach_m(conservative_cost_s);
        if distance_m <= corridor_m && angle_deg <= self.corridor_half_angle_deg() {
            return Some(3);
        }
        if distance_m <= guard_m * 1.45 && angle_deg <= 115.0 {
            return Some(4);
        }
        if matches!(self.phase, FlightPhase::Parked | FlightPhase::Taxi) && distance_m <= 60_000.0 {
            return Some(4);
        }
        None
    }

    pub fn candidate_dsf_cells(&self) -> Vec<(i32, i32)> {
        let conservative_cost_s = 1.0;
        let staging_m = if matches!(self.phase, FlightPhase::Parked | FlightPhase::Taxi) {
            60_000.0
        } else {
            0.0
        };
        let reach_m = self
            .corridor_reach_m(conservative_cost_s)
            .max(self.guard_radius_m())
            .max(staging_m);
        let dlat = (reach_m / 111_132.0).min(1.5);
        let cos_lat = self.lat.to_radians().cos().abs().max(0.1);
        let dlon = (reach_m / (111_320.0 * cos_lat)).min(1.5);
        let min_lat = (self.lat - dlat).floor() as i32;
        let max_lat = (self.lat + dlat).floor() as i32;
        let min_lon = (self.lon - dlon).floor() as i32;
        let max_lon = (self.lon + dlon).floor() as i32;
        let home = (self.lat.floor() as i32, self.lon.floor() as i32);
        let mut cells = vec![home];
        for lat in min_lat..=max_lat {
            for lon in min_lon..=max_lon {
                if (lat, lon) != home {
                    cells.push((lat, lon));
                }
            }
        }
        cells
    }
}

fn angular_diff_deg(a: f64, b: f64) -> f64 {
    let diff = (a - b).rem_euclid(360.0);
    if diff > 180.0 { 360.0 - diff } else { diff }
}

pub fn parse_rpos(packet: &[u8], received_at: Instant) -> Result<FlightSample> {
    ensure!(
        packet.len() >= RPOS_PACKET_LEN && packet.get(..4) == Some(b"RPOS"),
        "invalid RPOS packet header or length ({} bytes)",
        packet.len()
    );
    let lon = f64::from_le_bytes(packet[5..13].try_into()?);
    let lat = f64::from_le_bytes(packet[13..21].try_into()?);
    let elevation_msl_m = f64::from_le_bytes(packet[21..29].try_into()?);
    let agl_m = f32::from_le_bytes(packet[29..33].try_into()?);
    let pitch_deg = f32::from_le_bytes(packet[33..37].try_into()?);
    let heading_deg = f32::from_le_bytes(packet[37..41].try_into()?);
    let roll_deg = f32::from_le_bytes(packet[41..45].try_into()?);
    let vx_east_mps = f32::from_le_bytes(packet[45..49].try_into()?);
    let vy_up_mps = f32::from_le_bytes(packet[49..53].try_into()?);
    let vz_south_mps = f32::from_le_bytes(packet[53..57].try_into()?);
    let roll_rate_rad_s = f32::from_le_bytes(packet[57..61].try_into()?);
    let pitch_rate_rad_s = f32::from_le_bytes(packet[61..65].try_into()?);
    let yaw_rate_rad_s = f32::from_le_bytes(packet[65..69].try_into()?);

    ensure!(
        lat.is_finite() && (-90.0..=90.0).contains(&lat),
        "RPOS latitude out of range: {lat}"
    );
    ensure!(
        lon.is_finite() && (-180.0..=180.0).contains(&lon),
        "RPOS longitude out of range: {lon}"
    );
    ensure!(
        elevation_msl_m.is_finite() && (-1_000.0..=100_000.0).contains(&elevation_msl_m),
        "RPOS elevation out of range: {elevation_msl_m}"
    );
    ensure!(
        agl_m.is_finite() && (-500.0..=100_000.0).contains(&agl_m),
        "RPOS AGL altitude out of range: {agl_m}"
    );
    for value in [
        pitch_deg,
        heading_deg,
        roll_deg,
        vx_east_mps,
        vy_up_mps,
        vz_south_mps,
        roll_rate_rad_s,
        pitch_rate_rad_s,
        yaw_rate_rad_s,
    ] {
        if !value.is_finite() {
            bail!("RPOS kinematic field is non-finite");
        }
    }
    let ground_speed_mps = vx_east_mps.hypot(vz_south_mps);
    ensure!(
        ground_speed_mps <= 1_500.0 && vy_up_mps.abs() <= 500.0,
        "RPOS velocity exceeds physical bounds"
    );
    let normalized_heading = heading_deg.rem_euclid(360.0);
    let track_deg = if ground_speed_mps >= 2.0 {
        vx_east_mps
            .atan2(-vz_south_mps)
            .to_degrees()
            .rem_euclid(360.0)
    } else {
        normalized_heading
    };
    let turn_rate_deg_s = yaw_rate_rad_s.to_degrees().clamp(-45.0, 45.0);
    let phase = classify_phase(agl_m, ground_speed_mps, vy_up_mps);

    Ok(FlightSample {
        lat,
        lon,
        elevation_msl_m,
        agl_m,
        pitch_deg,
        heading_deg: normalized_heading,
        roll_deg,
        vx_east_mps,
        vy_up_mps,
        vz_south_mps,
        ground_speed_mps,
        track_deg,
        turn_rate_deg_s,
        phase,
        received_at,
    })
}

fn classify_phase(agl_m: f32, ground_speed_mps: f32, vy_up_mps: f32) -> FlightPhase {
    if agl_m < 15.0 {
        if ground_speed_mps < 1.5 {
            FlightPhase::Parked
        } else {
            FlightPhase::Taxi
        }
    } else if vy_up_mps < -2.5 {
        FlightPhase::Descent
    } else if agl_m < 3_000.0 || vy_up_mps > 2.5 {
        FlightPhase::Climb
    } else {
        FlightPhase::Cruise
    }
}

#[derive(Default)]
struct Inner {
    latest: Option<FlightSample>,
    packets: u64,
    invalid_packets: u64,
}

pub struct Telemetry {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Telemetry {
    pub fn start(on_update: Arc<dyn Fn(FlightSample) + Send + Sync>) -> Result<Self> {
        let target: SocketAddr = std::env::var("GROUNDEFFECT_XPLANE_UDP")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_XPLANE_UDP.to_owned())
            .parse()
            .context("invalid GROUNDEFFECT_XPLANE_UDP socket address")?;
        Self::start_with_target(target, on_update)
    }

    pub fn start_with_target(
        target: SocketAddr,
        on_update: Arc<dyn Fn(FlightSample) + Send + Sync>,
    ) -> Result<Self> {
        let bind_addr = if target.is_ipv4() {
            "127.0.0.1:0"
        } else {
            "[::1]:0"
        };
        let socket = UdpSocket::bind(bind_addr).context("binding local X-Plane RPOS UDP socket")?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let inner = Arc::new(Mutex::new(Inner::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_inner = Arc::clone(&inner);
        let thread_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("groundeffect-telemetry".into())
            .spawn(move || {
                let mut last_sub = Instant::now()
                    .checked_sub(SUBSCRIBE_INTERVAL)
                    .unwrap_or_else(Instant::now);
                let mut buf = [0_u8; 256];
                while !thread_stop.load(Ordering::Relaxed) {
                    if last_sub.elapsed() >= SUBSCRIBE_INTERVAL {
                        let _ = socket.send_to(b"RPOS\x002\x00", target);
                        last_sub = Instant::now();
                    }
                    match socket.recv_from(&mut buf) {
                        Ok((len, sender)) if sender.ip() == target.ip() => {
                            let now = Instant::now();
                            match parse_rpos(&buf[..len], now) {
                                Ok(sample) => {
                                    if let Ok(mut guard) = thread_inner.lock() {
                                        guard.latest = Some(sample);
                                        guard.packets += 1;
                                    }
                                    on_update(sample);
                                }
                                Err(_) => {
                                    if let Ok(mut guard) = thread_inner.lock() {
                                        guard.invalid_packets += 1;
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
                let _ = socket.send_to(b"RPOS\x000\x00", target);
            })?;
        Ok(Self {
            inner,
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }

    #[cfg(test)]
    pub fn inactive_for_test() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            stop: Arc::new(AtomicBool::new(true)),
            worker: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn record_for_test(&self, sample: FlightSample) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.latest = Some(sample);
            guard.packets += 1;
        }
    }

    pub fn snapshot(&self, now: Instant) -> TelemetrySnapshot {
        let Ok(guard) = self.inner.lock() else {
            return TelemetrySnapshot {
                state: TelemetryState::Unavailable,
                phase: FlightPhase::Unknown,
                sample: None,
                packets: 0,
                invalid_packets: 0,
            };
        };
        let state = match guard.latest {
            None => TelemetryState::Unavailable,
            Some(sample) if now.saturating_duration_since(sample.received_at) <= STALE_AFTER => {
                TelemetryState::Fresh
            }
            Some(_) => TelemetryState::Stale,
        };
        let phase = match (state, guard.latest) {
            (TelemetryState::Fresh, Some(sample)) => sample.phase,
            _ => FlightPhase::Unknown,
        };
        TelemetrySnapshot {
            state,
            phase,
            sample: guard.latest,
            packets: guard.packets,
            invalid_packets: guard.invalid_packets,
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn encode_rpos_packet(
    lon: f64,
    lat: f64,
    elevation_msl_m: f64,
    agl_m: f32,
    heading_deg: f32,
    vx_east_mps: f32,
    vy_up_mps: f32,
    vz_south_mps: f32,
    yaw_rate_rad_s: f32,
) -> [u8; RPOS_PACKET_LEN] {
    let mut out = [0_u8; RPOS_PACKET_LEN];
    out[..4].copy_from_slice(b"RPOS");
    out[4] = b'4';
    out[5..13].copy_from_slice(&lon.to_le_bytes());
    out[13..21].copy_from_slice(&lat.to_le_bytes());
    out[21..29].copy_from_slice(&elevation_msl_m.to_le_bytes());
    out[29..33].copy_from_slice(&agl_m.to_le_bytes());
    out[33..37].copy_from_slice(&0.0_f32.to_le_bytes());
    out[37..41].copy_from_slice(&heading_deg.to_le_bytes());
    out[41..45].copy_from_slice(&0.0_f32.to_le_bytes());
    out[45..49].copy_from_slice(&vx_east_mps.to_le_bytes());
    out[49..53].copy_from_slice(&vy_up_mps.to_le_bytes());
    out[53..57].copy_from_slice(&vz_south_mps.to_le_bytes());
    out[57..61].copy_from_slice(&0.0_f32.to_le_bytes());
    out[61..65].copy_from_slice(&0.0_f32.to_le_bytes());
    out[65..69].copy_from_slice(&yaw_rate_rad_s.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpos_parser_validates_bounds_phases_and_freshness() -> Result<()> {
        let t0 = Instant::now();
        let parked = parse_rpos(
            &encode_rpos_packet(-78.7875, 35.8776, 132.0, 2.1, 230.0, 0.0, 0.0, 0.0, 0.0),
            t0,
        )?;
        assert_eq!(parked.phase, FlightPhase::Parked);
        assert!((parked.track_deg - 230.0).abs() < 1e-4);

        let taxi = parse_rpos(
            &encode_rpos_packet(-78.7875, 35.8776, 132.0, 2.1, 230.0, 6.0, 0.0, 0.0, 0.0),
            t0,
        )?;
        assert_eq!(taxi.phase, FlightPhase::Taxi);
        assert!((taxi.track_deg - 90.0).abs() < 1e-4);

        let climb = parse_rpos(
            &encode_rpos_packet(
                -78.7875, 35.8776, 950.0, 820.0, 50.0, 45.0, 12.0, -45.0, 0.05,
            ),
            t0,
        )?;
        assert_eq!(climb.phase, FlightPhase::Climb);
        assert!((climb.track_deg - 45.0).abs() < 1e-3);

        let cruise = parse_rpos(
            &encode_rpos_packet(
                -78.7875, 35.8776, 10_668.0, 10_500.0, 0.0, 0.0, 0.2, -230.0, 0.0,
            ),
            t0,
        )?;
        assert_eq!(cruise.phase, FlightPhase::Cruise);
        assert!((cruise.track_deg - 0.0).abs() < 1e-3);

        let descent = parse_rpos(
            &encode_rpos_packet(
                -78.7875, 35.8776, 4_200.0, 4_050.0, 180.0, 0.0, -8.5, 160.0, 0.0,
            ),
            t0,
        )?;
        assert_eq!(descent.phase, FlightPhase::Descent);
        assert!((descent.track_deg - 180.0).abs() < 1e-3);

        assert!(parse_rpos(b"RPOSshort", t0).is_err());
        assert!(
            parse_rpos(
                &encode_rpos_packet(-78.7875, 95.0, 132.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0),
                t0
            )
            .is_err()
        );
        assert!(
            parse_rpos(
                &encode_rpos_packet(-78.7875, 35.8776, 132.0, 2.0, f32::NAN, 0.0, 0.0, 0.0, 0.0),
                t0
            )
            .is_err()
        );

        let telemetry = Telemetry::inactive_for_test();
        assert_eq!(telemetry.snapshot(t0).state, TelemetryState::Unavailable);
        telemetry.record_for_test(cruise);
        assert_eq!(
            telemetry.snapshot(t0 + Duration::from_secs(3)).state,
            TelemetryState::Fresh
        );
        assert_eq!(
            telemetry.snapshot(t0 + Duration::from_secs(6)).state,
            TelemetryState::Stale
        );
        Ok(())
    }

    #[test]
    fn udp_receiver_subscribes_parses_rpos_and_unsubscribes_on_shutdown() -> Result<()> {
        let fake_sim = UdpSocket::bind("127.0.0.1:0")?;
        fake_sim.set_read_timeout(Some(Duration::from_secs(2)))?;
        let sim_addr = fake_sim.local_addr()?;

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_cb = Arc::clone(&received);
        let telemetry = Telemetry::start_with_target(
            sim_addr,
            Arc::new(move |sample| {
                if let Ok(mut guard) = received_cb.lock() {
                    guard.push(sample);
                }
            }),
        )?;

        let mut buf = [0_u8; 64];
        let (len, client_addr) = fake_sim.recv_from(&mut buf)?;
        assert_eq!(&buf[..len], b"RPOS\x002\x00");

        fake_sim.send_to(b"RPOSbad", client_addr)?;
        let packet = encode_rpos_packet(-78.7875, 35.8776, 132.5, 1.8, 232.0, 0.0, 0.0, 0.0, 0.0);
        fake_sim.send_to(&packet, client_addr)?;

        for _ in 0..20 {
            if telemetry.snapshot(Instant::now()).packets >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let snap = telemetry.snapshot(Instant::now());
        assert_eq!(snap.state, TelemetryState::Fresh);
        assert_eq!(snap.packets, 1);
        assert_eq!(snap.invalid_packets, 1);
        assert_eq!(snap.sample.map(|s| s.phase), Some(FlightPhase::Parked));

        telemetry.shutdown();
        let (len, _) = fake_sim.recv_from(&mut buf)?;
        assert_eq!(&buf[..len], b"RPOS\x000\x00");
        Ok(())
    }

    #[test]
    fn kinematics_prioritize_forward_corridor_and_flag_behind_aircraft() -> Result<()> {
        let sample = parse_rpos(
            &encode_rpos_packet(
                -78.7875, 35.8776, 3_500.0, 3_350.0, 0.0, 0.0, 0.0, -180.0, 0.0,
            ),
            Instant::now(),
        )?;
        let kin = ReferenceKinematics::from_sample(&sample);
        let near_rdu = Footprint::parse("25760_18432_BI16.dds")?;
        let ahead_north = Footprint::parse("25600_18432_BI16.dds")?;
        let behind_south = Footprint::parse("26080_18432_BI16.dds")?;

        assert!(kin.is_protected(near_rdu));
        assert!(!kin.is_behind(near_rdu));
        assert!(!kin.is_behind(ahead_north));
        assert!(kin.is_behind(behind_south));

        let ahead_class = kin
            .classify_lookahead(ahead_north)
            .context("ahead footprint should be in corridor")?;
        assert!(ahead_class <= 3);
        assert!(kin.classify_lookahead(behind_south).is_none());
        Ok(())
    }
}
