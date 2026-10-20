//! The modem for Cuttlefish's radio HAL (android.hardware.radio-service.cuttlefish),
//! whose reference RIL speaks AT commands to Cuttlefish's host modem simulator
//! over vsock and exits without one. With no radio HAL, Android's phone
//! process waits for it at every start, is killed for not responding and
//! starts again, all the time.
//!
//! Until the control socket says otherwise it answers the way a modem with no
//! SIM inserted and no network does, so telephony settles on "No SIM" and the
//! device behaves like a Wi-Fi-only phone. `cellular <type> <bars>` puts in a
//! test SIM (PLMN 001/01, named Avo; the image's APN list has APNs for it)
//! registered at home on that network type with that many signal bars, and
//! mobile data works: a data call gets 10.0.2.15/24, gateway 10.0.2.2 and DNS
//! 10.0.2.3 on Cuttlefish's eth0 (buried_eth0), which has a user-mode network
//! of its own (net.rs, cuttlefish.rs). `cellular off` takes the SIM out again.
//! Calls and SMS fail.
//!
//! The answers are what the reference RIL reads (reference-ril.c and at_tok.c
//! in device/google/cuttlefish), mostly as Cuttlefish's modem simulator gives
//! them. A change reaches a running Android as unsolicited lines: a new
//! network type as `+CREG:` (the RIL then reports a new network state, and
//! Android asks for registration and operator again) and `+CSQ:`, new bars as
//! `+CSQ:`. The RIL reads the SIM's status only when the radio's state
//! changes, so a SIM going in or out is `+CFUN: 0`: Android turns the radio on
//! again at once and finds the SIM, or that it is gone, and ends a data call
//! the usual way. (Closing the connection would do too, but Android then drops
//! the data call without telling the RIL, which keeps the call's id for good;
//! it has 11.) With the radio off (airplane mode) Android finds out when it
//! turns the radio on.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// The vsock port the radio HAL dials (androidboot.modem_simulator_ports).
pub const PORT: u32 = 9600;

/// The test SIM and its network: PLMN 001/01, ITU-T's test network.
const PLMN: &str = "00101";
const OPERATOR: &str = "Avo";
const IMSI: &str = "001010123456789";
const ICCID: &str = "89001010000000000018";
const MSISDN: &str = "+15555550100";
/// The radio HAL answers Android's getImei itself, with this one
/// (RefRadioModem.cpp); AT+CGSN gives the same.
const IMEI: &str = "867400022047199";
/// The cell, in +CREG's hex: the modem simulator's location area and cell id.
const LAC: &str = "2142";
const CELL: &str = "0000B804";
/// A data call's address: the guest's on its network (net.rs).
const ADDRESS: &str = "10.0.2.15/24";
/// The RIL's modem technologies this modem has (AT+CTEC: GSM, WCDMA, LTE,
/// NR, as bit numbers of its MDM_* masks) and the preferred ones, all of
/// them (net2pmask's "NR 5G, LTE, GSM and WCDMA").
const TECHNOLOGIES: &str = "0,1,5,6";
const PREFERRED: u32 = 0x63;
/// PDP context ids the RIL uses (MAX_PDP).
const CONTEXTS: u32 = 11;
/// An unknown measure in +CSQ.
const UNKNOWN: i32 = i32::MAX;

/// Measures Android shows as 0..=4 bars, by the default thresholds of
/// CarrierConfigManager and CellSignalStrengthWcdma: GSM RSSI in ASU
/// (-113 + 2 × ASU dBm: -111, -105, -101, -93 and -73 against -107, -103,
/// -97 and -89), WCDMA RSSI in ASU (-111, -103, -93, -83 and -69 against
/// -107, -97, -87 and -77), LTE RSRP (against -128, -118, -108 and -98) and
/// NR SS-RSRP (against -110, -90, -80 and -65) in dBm × -1, as the RIL
/// carries them.
const GSM_ASU: [i32; 5] = [1, 4, 6, 10, 20];
const WCDMA_ASU: [i32; 5] = [1, 5, 10, 15, 22];
const LTE_RSRP: [i32; 5] = [135, 123, 113, 103, 90];
const NR_SS_RSRP: [i32; 5] = [115, 100, 85, 72, 60];

/// A guest that stops reading holds up its connection, and `cellular`, no
/// longer than this.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A network type the SIM can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rat {
    Gprs,
    Edge,
    Umts,
    Hspa,
    Lte,
    Nr,
}

impl Rat {
    const NAMES: [(&'static str, Rat); 6] =
        [("gprs", Rat::Gprs), ("edge", Rat::Edge), ("umts", Rat::Umts), ("hspa", Rat::Hspa), ("lte", Rat::Lte), ("nr", Rat::Nr)];

    /// `<AcT>` of +CREG, +CGREG and +CEREG, which the RIL turns into Android's
    /// radio technology (mapNetworkRegistrationResponse): GPRS, EDGE, UMTS,
    /// HSPA, LTE, NR (standalone).
    fn act(self) -> u8 {
        match self {
            Rat::Gprs => 0,
            Rat::Edge => 3,
            Rat::Umts => 2,
            Rat::Hspa => 6,
            Rat::Lte => 7,
            Rat::Nr => 11,
        }
    }

    /// The RIL's modem technology (AT+CTEC?). It asks for data registration
    /// with AT+CEREG on LTE and AT+CGREG otherwise, which answer alike.
    fn technology(self) -> u8 {
        match self {
            Rat::Gprs | Rat::Edge => 0,
            Rat::Umts | Rat::Hspa => 1,
            Rat::Lte => 5,
            Rat::Nr => 6,
        }
    }
}

/// The network the SIM is registered on, at home, and its signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    pub rat: Rat,
    /// 0..=4, as Android's status bar shows them.
    pub bars: u8,
}

impl Service {
    /// `<gprs|edge|umts|hspa|lte|nr> <bars 0-4>`, or `off` (no SIM).
    pub fn parse(words: &[&str]) -> Result<Option<Service>, String> {
        if words == ["off"] {
            return Ok(None);
        }
        let [rat, bars] = words else { return Err("cellular needs a network type and bars, or off".into()) };
        let rat = Rat::NAMES.iter().find(|(name, _)| name == rat).map(|(_, rat)| *rat).ok_or(format!("bad network type {rat}"))?;
        let bars = bars.parse().ok().filter(|bars| *bars <= 4).ok_or(format!("bad bars {bars}"))?;
        Ok(Some(Service { rat, bars }))
    }

    /// +CSQ's fields, the RIL's RIL_SignalStrength_v12 in order (GSM, CDMA,
    /// EVDO, LTE, TD-SCDMA, WCDMA, NR): this network type's measure for the
    /// bars and the rest unknown, as the modem simulator gives them.
    fn signal(self) -> [i32; 22] {
        let mut fields = [UNKNOWN; 22];
        let bars = self.bars.min(4) as usize;
        match self.rat {
            Rat::Gprs | Rat::Edge => {
                fields[0] = GSM_ASU[bars];
                fields[1] = 99; // bit error rate unknown
            }
            Rat::Umts | Rat::Hspa => {
                fields[14] = WCDMA_ASU[bars];
                fields[15] = 99;
            }
            Rat::Lte => {
                fields[7] = 99; // RSSI unknown: Android's LTE bars go by RSRP
                fields[8] = LTE_RSRP[bars];
            }
            Rat::Nr => fields[16] = NR_SS_RSRP[bars],
        }
        fields
    }
}

/// The SIM's network as the control socket set it (`cellular`), shared with
/// the modem's connections. It lasts as long as the process, through guest
/// reboots.
pub static CELLULAR: LazyLock<Cellular> = LazyLock::new(Cellular::default);

#[derive(Default)]
pub struct Cellular {
    state: Mutex<Shared>,
}

#[derive(Default)]
struct Shared {
    service: Option<Service>,
    sessions: Vec<(u64, Arc<Mutex<Session>>)>,
    next: u64,
}

/// One connection from the radio HAL.
struct Session {
    modem: Modem,
    /// Answers and unsolicited lines go out here, each whole at a time.
    stream: UnixStream,
}

impl Cellular {
    /// Sets the SIM's network (`None`: no SIM) and tells the radio HAL.
    pub fn set(&self, service: Option<Service>) {
        let mut shared = self.state.lock().unwrap();
        shared.service = service;
        for (_, session) in &shared.sessions {
            let mut session = session.lock().unwrap();
            let lines = session.modem.tell(service);
            if !lines.is_empty() && session.stream.write_all(lines.as_bytes()).is_err() {
                let _ = session.stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// A new connection, which starts with the network as it is now.
    fn join(&self, stream: UnixStream) -> (u64, Arc<Mutex<Session>>) {
        let mut shared = self.state.lock().unwrap();
        let modem = Modem { service: shared.service, ..Modem::default() };
        let session = Arc::new(Mutex::new(Session { modem, stream }));
        shared.next += 1;
        let id = shared.next;
        shared.sessions.push((id, session.clone()));
        (id, session)
    }

    fn leave(&self, id: u64) {
        self.state.lock().unwrap().sessions.retain(|(other, _)| *other != id);
    }
}

/// Answers modem connections from the guest on `path`, the vsock device's
/// socket for guest connections to [`PORT`]. Once per process: the modem
/// keeps nothing of a connection after it, and a listener made again at every
/// guest reboot would leave the last one's thread waiting on a socket nobody
/// reaches.
pub fn serve(path: &Path) -> Result<(), String> {
    static SERVING: AtomicBool = AtomicBool::new(false);
    if SERVING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("modem: {}: {e}", path.display()))?;
    std::thread::Builder::new()
        .name("avo-modem".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = std::thread::Builder::new().name("avo-modem-session".into()).spawn(move || session(stream, &CELLULAR));
            }
        })
        .map_err(|e| format!("modem: {e}"))?;
    Ok(())
}

fn session(stream: UnixStream, cellular: &Cellular) {
    let Ok(reader) = stream.try_clone() else { return };
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut reader = BufReader::new(reader);
    let (id, shared) = cellular.join(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\r', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let command = String::from_utf8_lossy(&line);
        let command = command.trim_matches(|c: char| c.is_whitespace() || c == '\0');
        if command.is_empty() {
            continue;
        }
        let mut session = shared.lock().unwrap();
        let answer = session.modem.answer(command);
        if session.stream.write_all(answer.as_bytes()).is_err() {
            break;
        }
    }
    cellular.leave(id);
}

const OK: &str = "OK\r\n";

/// Intermediate lines, then OK.
fn ok<S: AsRef<str>>(lines: &[S]) -> String {
    lines.iter().map(|line| format!("{}\r\n", line.as_ref())).collect::<String>() + OK
}

#[derive(Default)]
struct Modem {
    /// AT+CFUN: Android has the radio on.
    powered: bool,
    /// The SIM's network as this connection last heard of it (`tell`).
    service: Option<Service>,
    /// `<n>` of AT+CREG, AT+CGREG and AT+CEREG: 0 for no unsolicited reports.
    reports: [u8; 3],
    /// PDP contexts by id (AT+CGDCONT): PDP type and APN.
    contexts: BTreeMap<u32, (String, String)>,
}

impl Modem {
    fn answer(&mut self, command: &str) -> String {
        let upper = command.to_ascii_uppercase();
        if let Some(value) = upper.strip_prefix("AT+CFUN=") {
            self.powered = value.split(',').next().is_some_and(|v| v.trim() != "0");
        }
        for (report, prefix) in ["AT+CREG=", "AT+CGREG=", "AT+CEREG="].into_iter().enumerate() {
            if let Some(n) = upper.strip_prefix(prefix) {
                self.reports[report] = n.trim().parse().unwrap_or(0);
            }
        }
        match self.service {
            None => self.without_sim(&upper),
            Some(service) => self.with_sim(service, command, &upper),
        }
    }

    fn without_sim(&self, upper: &str) -> String {
        // SIM-related commands: there is no SIM (27.007 CME error 10).
        const SIM: [&str; 8] = ["AT+CPIN", "AT+CIMI", "AT+CRSM", "AT+CSIM", "AT+CNUM", "AT+CCHO", "AT+CGLA", "AT+CLCK"];
        if SIM.iter().any(|prefix| upper.starts_with(prefix)) {
            return "+CME ERROR: 10\r\n".into();
        }
        match upper {
            "AT+CFUN?" => ok(&[format!("+CFUN: {}", self.powered as u8)]),
            "AT+CREG?" => ok(&["+CREG: 2,0"]),
            "AT+CGREG?" => ok(&["+CGREG: 2,0"]),
            "AT+CEREG?" => ok(&["+CEREG: 2,0"]),
            "AT+COPS?" => ok(&["+COPS: 0"]),
            "AT+CSQ" => ok(&["+CSQ: 99,99"]),
            // No IMEI or serial to give: an empty modem has none worth inventing.
            "AT+CGSN" | "AT+CGMI" | "AT+CGMM" | "AT+CGMR" => "+CME ERROR: 4\r\n".into(),
            _ => OK.into(),
        }
    }

    fn with_sim(&mut self, service: Service, command: &str, upper: &str) -> String {
        const OPERATOR_NAMES: &str = "AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?";
        let registered = self.powered;
        match upper {
            // The RIL's set-up (initializeCallback), a data call's
            // (requestSetupDataCall; ATD*99***1# starts data) and no calls.
            "ATE0Q0V1" | "ATS0=0" | "AT+CMEE=1" | "AT+CCWA=1" | "AT+CMOD=0" | "AT+CSSN=0,1" | "AT+COLP=0" | "AT+CSCS=\"HEX\"" | "AT+CUSD=1"
            | "AT+CGEREP=1,0" | "AT+CMGF=0" | "AT+CNMI=1,2,2,1,1" | "AT+CGQREQ=1" | "AT+CGQMIN=1" | "ATD*99***1#" | "AT+CLCC" => OK.into(),
            "AT+CPIN?" => ok(&["+CPIN: READY"]),
            // The RIL takes the first line that starts with a digit.
            "AT+CICCID" => ok(&[ICCID]),
            "AT+CIMI" => ok(&[IMSI]),
            "AT+CGSN" => ok(&[IMEI]),
            "AT+CFUN?" => ok(&[format!("+CFUN: {}", self.powered as u8)]),
            "AT+CREG?" => ok(&[self.registration("+CREG", 0)]),
            "AT+CGREG?" => ok(&[self.registration("+CGREG", 1)]),
            "AT+CEREG?" => ok(&[self.registration("+CEREG", 2)]),
            // Automatic selection (requestQueryNetworkSelectionMode reads the mode).
            "AT+COPS?" if registered => ok(&[format!("+COPS: 0,2,\"{PLMN}\"")]),
            "AT+COPS?" => ok(&["+COPS: 0"]),
            // requestOperator: long and short name and the numeric PLMN, whose
            // five digits say the MNC has two; "+COPS: 0" each unregistered.
            OPERATOR_NAMES if registered => {
                ok(&[format!("+COPS: 0,0,\"{OPERATOR}\""), format!("+COPS: 0,1,\"{OPERATOR}\""), format!("+COPS: 0,2,\"{PLMN}\"")])
            }
            OPERATOR_NAMES => ok(&["+COPS: 0"; 3]),
            "AT+CSQ" => ok(&[self.signal()]),
            "AT+CTEC?" => ok(&[format!("+CTEC: {},{PREFERRED:x}", service.rat.technology())]),
            "AT+CTEC=?" => ok(&[format!("+CTEC: {TECHNOLOGIES}")]),
            // requestGetMute reads a line after an error, so no error here.
            "AT+CMUT?" => ok(&["+CMUT: 0"]),
            // requestOrSendDataCallList reads the call with id n from the
            // n-th line: one per id up to the highest, ids without a context
            // not active.
            "AT+CGACT?" => {
                let last = self.contexts.keys().next_back().copied().unwrap_or(0);
                ok(&(1..=last).map(|id| format!("+CGACT: {id},{}", self.contexts.contains_key(&id) as u8)).collect::<Vec<_>>())
            }
            // The address of a context goes on buried_eth0 (set_Ip_Addr).
            "AT+CGDCONT?" => ok(&self
                .contexts
                .iter()
                .map(|(id, (kind, apn))| format!("+CGDCONT: {id},\"{kind}\",\"{apn}\",\"{ADDRESS}\",0,0"))
                .collect::<Vec<_>>()),
            _ => self.with_sim_set(command, upper),
        }
    }

    /// Commands with parameters, for `with_sim`.
    fn with_sim_set(&mut self, command: &str, upper: &str) -> String {
        const SETTINGS: [&str; 7] = ["AT+CFUN=", "AT+CREG=", "AT+CGREG=", "AT+CEREG=", "AT+COPS=", "AT+CGACT=", "AT+CMUT="];
        if SETTINGS.iter().any(|prefix| upper.starts_with(prefix)) {
            return OK.into();
        }
        // to_ascii_uppercase keeps byte offsets, so a prefix of `upper` is one of `command`.
        let after = |prefix: &str| upper.starts_with(prefix).then(|| parameters(&command[prefix.len()..]));
        if upper.starts_with("AT+CTEC=") {
            // requestSetPreferredNetworkType: the network type stays the control socket's.
            return ok(&["+CTEC: DONE"]);
        }
        if let Some(fields) = after("AT+CGDCONT=") {
            let Some(id) = fields[0].trim().parse().ok().filter(|id| (1..=CONTEXTS).contains(id)) else {
                return "+CME ERROR: 50\r\n".into();
            };
            if fields.len() == 1 {
                self.contexts.remove(&id);
            } else {
                let field = |i: usize| fields.get(i).cloned().unwrap_or_default();
                self.contexts.insert(id, (field(1), field(2)));
            }
            return OK.into();
        }
        if let Some(fields) = after("AT+CGCONTRDP=") {
            let id = fields[0].trim().parse().unwrap_or(0);
            let Some((_, apn)) = self.contexts.get(&id) else { return "+CME ERROR: 21\r\n".into() };
            // Bearer 5, the first EPS bearer; gateway and DNS server of net.rs.
            let (gateway, dns) = (crate::net::GATEWAY, crate::net::DNS);
            return ok(&[format!("+CGCONTRDP: {id},5,\"{apn}\",\"{ADDRESS}\",\"{gateway}\",\"{dns}\"")]);
        }
        if let Some(fields) = after("AT+CRSM=") {
            return ok(&[sim_io(&fields)]);
        }
        if let Some(fields) = after("AT+CLCK=") {
            // requestFacilityLock: a query (mode 2) reads one "+CLCK: <status>"
            // line. No lock is on (the SIM's PIN, fixed dialling, barring),
            // and none can be set.
            return match fields.get(1).map(|mode| mode.trim()) {
                Some("2") => ok(&["+CLCK: 0"]),
                _ => "+CME ERROR: 3\r\n".into(),
            };
        }
        // Calls, SMS, USSD, supplementary services, SIM Toolkit, logical
        // channels: what the modem simulator answers to what it does not know.
        "+CME ERROR: 4\r\n".into()
    }

    /// +CREG, +CGREG or +CEREG: `<n>,<stat>[,"<lac>","<ci>",<AcT>]`,
    /// registered at home with four commas, the form parseRegistrationState
    /// reads a network type from, as an answer or an unsolicited line.
    fn registration(&self, prefix: &str, report: usize) -> String {
        let n = self.reports[report];
        match self.service.filter(|_| self.powered) {
            Some(service) => format!("{prefix}: {n},1,\"{LAC}\",\"{CELL}\",{}", service.rat.act()),
            None => format!("{prefix}: {n},0"),
        }
    }

    fn signal(&self) -> String {
        let fields = self.service.filter(|_| self.powered).map_or([UNKNOWN; 22], Service::signal);
        format!("+CSQ: {}", fields.map(|field| field.to_string()).join(","))
    }

    /// Unsolicited lines that tell the RIL the SIM's network is now `service`.
    fn tell(&mut self, service: Option<Service>) -> String {
        let before = std::mem::replace(&mut self.service, service);
        if before == service || !self.powered {
            // With the radio off it reads everything again when it is turned on.
            return String::new();
        }
        if before.is_none() || service.is_none() {
            // The SIM went in or out: the radio goes off (setRadioState), so
            // Android asks for the SIM's status, and turns it on again.
            self.powered = false;
            return "+CFUN: 0\r\n".into();
        }
        let mut lines = String::new();
        if before.map(|b| b.rat) != service.map(|s| s.rat) && self.reports[0] != 0 {
            lines += &format!("{}\r\n", self.registration("+CREG", 0));
        }
        lines + &self.signal() + "\r\n"
    }
}

/// The comma-separated parameters of a command, without their quotes.
fn parameters(text: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

/// An elementary file of the SIM: transparent, or records of one length.
enum Ef {
    Transparent(Vec<u8>),
    Records(Vec<Vec<u8>>),
}

impl Ef {
    /// The files Android reads that the SIM has, by id. The RIL sends no
    /// path, and the USIM's EF_IMSI (6F07) shares its id with the ISIM's
    /// EF_IST: the IMSI comes from AT+CIMI only.
    fn find(id: i64) -> Option<Ef> {
        Some(match id {
            0x2FE2 => Ef::Transparent(swapped_bcd(ICCID)), // EF_ICCID
            0x6FAD => Ef::Transparent(vec![0x00, 0x00, 0x00, 0x02]), // EF_AD: normal operation, MNC of two digits
            0x6F46 => Ef::Transparent(service_provider_name(OPERATOR)), // EF_SPN
            0x6F40 => Ef::Records(vec![dialling_number(MSISDN)]), // EF_MSISDN
            _ => return None,
        })
    }

    fn size(&self) -> usize {
        match self {
            Ef::Transparent(data) => data.len(),
            Ef::Records(records) => records.iter().map(Vec::len).sum(),
        }
    }

    /// GET RESPONSE: a USIM FCP template (TS 102 221 11.1.1.3), which the
    /// RIL turns into the SIM format Android reads (convertUsimToSim): the
    /// file descriptor first (with the record length), the size last.
    fn template(&self, id: u16) -> Vec<u8> {
        let mut body = match self {
            Ef::Transparent(_) => vec![0x82, 0x02, 0x41, 0x21],
            Ef::Records(records) => vec![0x82, 0x05, 0x42, 0x21, 0x00, records[0].len() as u8, records.len() as u8],
        };
        let [id_high, id_low] = id.to_be_bytes();
        let [size_high, size_low] = (self.size() as u16).to_be_bytes();
        // File id, life cycle (operational), security attributes, file size.
        body.extend([0x83, 0x02, id_high, id_low, 0x8A, 0x01, 0x05, 0x8B, 0x03, 0x6F, 0x06, 0x01, 0x80, 0x02, size_high, size_low]);
        let mut template = vec![0x62, body.len() as u8];
        template.extend(body);
        template
    }
}

/// AT+CRSM's `<command>,<fileid>,<P1>,<P2>,<P3>[,<data>]` as a +CRSM line
/// (status words, then the data in hex): "106,130" (6A82, file not found)
/// for files the SIM has not.
fn sim_io(fields: &[String]) -> String {
    const READ_BINARY: i64 = 176;
    const READ_RECORD: i64 = 178;
    const GET_RESPONSE: i64 = 192;
    const UPDATE_BINARY: i64 = 214;
    const UPDATE_RECORD: i64 = 220;
    let number = |i: usize| fields.get(i).and_then(|f| f.trim().parse::<i64>().ok()).unwrap_or(-1);
    let (command, id, p1, p2, p3) = (number(0), number(1), number(2), number(3), number(4));
    let Some(file) = Ef::find(id) else { return "+CRSM: 106,130".into() };
    let data = |bytes: &[u8]| format!("+CRSM: 144,0,{}", bytes.iter().map(|b| format!("{b:02X}")).collect::<String>());
    match (command, &file) {
        (GET_RESPONSE, _) => data(&file.template(id as u16)),
        (READ_BINARY, Ef::Transparent(bytes)) => {
            let offset = ((p1.max(0) << 8) | p2.max(0)) as usize;
            if offset > bytes.len() {
                return "+CRSM: 107,0".into(); // 6B00: wrong offset
            }
            let end = if p3 <= 0 { bytes.len() } else { (offset + p3 as usize).min(bytes.len()) };
            data(&bytes[offset..end])
        }
        (READ_RECORD, Ef::Records(records)) => match usize::try_from(p1 - 1).ok().and_then(|i| records.get(i)) {
            Some(record) => data(record),
            None => "+CRSM: 106,131".into(), // 6A83: record not found
        },
        // Taken and forgotten: Android keeps what it wrote itself.
        (UPDATE_BINARY | UPDATE_RECORD, _) => "+CRSM: 144,0".into(),
        _ => "+CRSM: 106,130".into(),
    }
}

/// Digits two to a byte, the first in the low nibble, an odd one out padded
/// with F (EF_ICCID; BCD numbers of TS 24.008).
fn swapped_bcd(digits: &str) -> Vec<u8> {
    let digits: Vec<u8> = digits.bytes().filter(u8::is_ascii_digit).map(|d| d - b'0').collect();
    digits.chunks(2).map(|pair| pair.get(1).copied().unwrap_or(0xF) << 4 | pair[0]).collect()
}

/// EF_SPN: the display condition (the name alone on the home network), then
/// the name in the GSM default alphabet, which letters share with ASCII,
/// padded with FF to 16.
fn service_provider_name(name: &str) -> Vec<u8> {
    let mut data = vec![0x00];
    data.extend(name.bytes().take(16));
    data.resize(17, 0xFF);
    data
}

/// An EF_MSISDN record, coded like an EF_ADN one (TS 31.102 4.4.2.3): no
/// alpha identifier, the number's length, type (international) and up to 20
/// digits, no capability or extension record.
fn dialling_number(number: &str) -> Vec<u8> {
    let digits = swapped_bcd(number);
    let mut record = vec![0xFF; 14];
    record.push(digits.len() as u8 + 1);
    record.push(if number.starts_with('+') { 0x91 } else { 0x81 });
    record.extend(&digits);
    record.resize(14 + 2 + 10, 0xFF);
    record.extend([0xFF, 0xFF]);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_like_a_modem_without_a_sim() {
        let mut modem = Modem::default();
        assert_eq!(modem.answer("ATE0Q0V1"), "OK\r\n");
        assert_eq!(modem.answer("AT+CPIN?"), "+CME ERROR: 10\r\n");
        assert_eq!(modem.answer("AT+CFUN?"), "+CFUN: 0\r\nOK\r\n");
        assert_eq!(modem.answer("AT+CFUN=1"), "OK\r\n");
        assert_eq!(modem.answer("at+cfun?"), "+CFUN: 1\r\nOK\r\n");
        assert_eq!(modem.answer("AT+CREG?"), "+CREG: 2,0\r\nOK\r\n");
    }

    /// What at_tok.c makes of an intermediate line: the text after the first
    /// ':' (at_tok_start), then one token per comma, a quoted one up to its
    /// closing quote with the rest up to the next comma skipped (nextTok).
    fn tokens(line: &str) -> Vec<String> {
        let mut rest = &line[line.find(':').unwrap() + 1..];
        let mut tokens = Vec::new();
        loop {
            rest = rest.trim_start();
            if let Some(quoted) = rest.strip_prefix('"') {
                let end = quoted.find('"').unwrap();
                tokens.push(quoted[..end].to_string());
                let Some(comma) = quoted[end..].find(',') else { break };
                rest = &quoted[end + comma + 1..];
            } else {
                match rest.split_once(',') {
                    Some((token, after)) => {
                        tokens.push(token.to_string());
                        rest = after;
                    }
                    None => {
                        tokens.push(rest.to_string());
                        break;
                    }
                }
            }
        }
        tokens
    }

    /// The lines of an answer that ends in OK, without it.
    fn lines(answer: &str) -> Vec<&str> {
        let lines: Vec<&str> = answer.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.last(), Some(&"OK"), "{answer:?}");
        lines[..lines.len() - 1].to_vec()
    }

    fn line(answer: &str) -> String {
        let lines = lines(answer);
        assert_eq!(lines.len(), 1, "{answer:?}");
        lines[0].to_string()
    }

    fn with_sim(rat: Rat, bars: u8) -> Modem {
        let mut modem = Modem { service: Some(Service { rat, bars }), ..Modem::default() };
        for command in ["ATE0Q0V1", "AT+CMEE=1", "AT+CREG=2", "AT+CGREG=1", "AT+CFUN=1"] {
            assert_eq!(modem.answer(command), OK, "{command}");
        }
        modem
    }

    #[test]
    fn cellular_parses_a_network_type_and_bars() {
        assert_eq!(Service::parse(&["lte", "4"]), Ok(Some(Service { rat: Rat::Lte, bars: 4 })));
        assert_eq!(Service::parse(&["edge", "0"]), Ok(Some(Service { rat: Rat::Edge, bars: 0 })));
        assert_eq!(Service::parse(&["off"]), Ok(None));
        assert!(Service::parse(&["lte", "5"]).is_err());
        assert!(Service::parse(&["lte"]).is_err());
        assert!(Service::parse(&["wimax", "3"]).is_err());
        assert!(Service::parse(&[]).is_err());
    }

    #[test]
    fn the_sim_is_ready_and_registered_at_home() {
        let mut modem = with_sim(Rat::Lte, 4);
        assert_eq!(tokens(&line(&modem.answer("AT+CPIN?"))), ["READY"]);
        // AT+CICCID, AT+CIMI and AT+CGSN are numeric: a line starting with a digit.
        assert_eq!(line(&modem.answer("AT+CICCID")), ICCID);
        assert_eq!(line(&modem.answer("AT+CIMI")), IMSI);
        assert_eq!(line(&modem.answer("AT+CGSN")), IMEI);
        assert!(IMSI.starts_with(PLMN));
        // parseRegistrationState: four commas are <n>,<stat>,<lac>,<cid>,<AcT>,
        // with lac and cid in hex.
        for (command, n) in [("AT+CREG?", "2"), ("AT+CGREG?", "1"), ("AT+CEREG?", "0")] {
            let fields = tokens(&line(&modem.answer(command)));
            assert_eq!(fields, [n, "1", "2142", "0000B804", "7"], "{command}");
            assert_eq!(i64::from_str_radix(&fields[3], 16), Ok(0xB804));
        }
        // requestOperator wants three lines; numeric last.
        let operator = modem.answer("AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?");
        let operator: Vec<Vec<String>> = lines(&operator).iter().map(|l| tokens(l)).collect();
        assert_eq!(operator, [["0", "0", "Avo"], ["0", "1", "Avo"], ["0", "2", "00101"]]);
        assert_eq!(tokens(&line(&modem.answer("AT+COPS?")))[0], "0");
        // probeForModemMode: current technology (LTE, bit 5) and preferred
        // mask in hex, which must be one of net2pmask's; supported bits.
        assert_eq!(tokens(&line(&modem.answer("AT+CTEC?"))), ["5", "63"]);
        assert_eq!(tokens(&line(&modem.answer("AT+CTEC=?"))), ["0", "1", "5", "6"]);
        assert_eq!(line(&modem.answer("AT+CTEC=5,\"23\"")), "+CTEC: DONE");
        // Multiline answers the RIL reads a first line of, and the mute it
        // reads one after an error of.
        assert_eq!(tokens(&line(&modem.answer("AT+CLCK=\"SC\",2,\"\",7"))), ["0"]);
        assert_eq!(tokens(&line(&modem.answer("AT+CMUT?"))), ["0"]);
        assert_eq!(modem.answer("AT+CLCC"), OK, "no calls");
        assert!(modem.answer("ATD5551234;").starts_with("+CME ERROR"), "no calls either");
        assert!(modem.answer("AT+CPINR=\"SIM PIN\"").starts_with("+CME ERROR"), "a multiline answer with no line would crash the RIL");
        // The radio off: not registered, no operator, no signal.
        modem.answer("AT+CFUN=0");
        assert_eq!(tokens(&line(&modem.answer("AT+CREG?"))), ["2", "0"]);
        assert_eq!(lines(&modem.answer("AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?")), ["+COPS: 0"; 3]);
        assert!(tokens(&line(&modem.answer("AT+CSQ"))).iter().all(|f| f == &UNKNOWN.to_string()));
    }

    /// Android's level for a +CSQ line (RILUtils and CellSignalStrength*, the
    /// thresholds of CarrierConfigManager), and the network it reads it for.
    fn android_level(line: &str) -> (Option<&'static str>, i32) {
        let fields: Vec<i32> = tokens(line).iter().map(|f| f.parse().unwrap()).collect();
        assert_eq!(fields.len(), 22, "RIL_SignalStrength_v12");
        let dbm_from_asu = |asu: i32| (0..=31).contains(&asu).then(|| -113 + 2 * asu);
        let level = |measure: i32, thresholds: [i32; 4]| thresholds.iter().filter(|t| measure >= **t).count() as i32;
        // SignalStrength.getPrimary: NR, LTE, ..., WCDMA, GSM.
        let nr = Some(-fields[16]).filter(|rsrp| (-156..=-31).contains(rsrp));
        let lte = Some(-fields[8]).filter(|rsrp| (-140..=-44).contains(rsrp));
        let wcdma = dbm_from_asu(fields[14]);
        let gsm = dbm_from_asu(fields[0]);
        if let Some(rsrp) = nr {
            return (Some("nr"), level(rsrp, [-110, -90, -80, -65]));
        }
        if let Some(rsrp) = lte {
            return (Some("lte"), level(rsrp, [-128, -118, -108, -98]));
        }
        if let Some(rssi) = wcdma {
            return (Some("wcdma"), level(rssi, [-107, -97, -87, -77]));
        }
        if let Some(rssi) = gsm {
            return (Some("gsm"), level(rssi, [-107, -103, -97, -89]));
        }
        (None, 0)
    }

    #[test]
    fn bars_are_what_android_shows() {
        let kinds =
            [(Rat::Gprs, "gsm"), (Rat::Edge, "gsm"), (Rat::Umts, "wcdma"), (Rat::Hspa, "wcdma"), (Rat::Lte, "lte"), (Rat::Nr, "nr")];
        for (rat, kind) in kinds {
            for bars in 0..=4 {
                let mut modem = with_sim(rat, bars);
                assert_eq!(android_level(&line(&modem.answer("AT+CSQ"))), (Some(kind), bars as i32), "{rat:?} {bars}");
            }
        }
        // And each network type comes to Android as its radio technology.
        for (rat, act) in [(Rat::Gprs, "0"), (Rat::Edge, "3"), (Rat::Umts, "2"), (Rat::Hspa, "6"), (Rat::Lte, "7"), (Rat::Nr, "11")] {
            let mut modem = with_sim(rat, 2);
            assert_eq!(tokens(&line(&modem.answer("AT+CGREG?")))[4], act);
            // Never CDMA (bit 2, or 4 and 8, which the RIL takes for 1xRTT
            // and EvDo): GSM, WCDMA, LTE or NR.
            assert!(["0", "1", "5", "6"].contains(&tokens(&line(&modem.answer("AT+CTEC?")))[0].as_str()));
        }
    }

    #[test]
    fn a_data_call_gets_the_address_gateway_and_dns() {
        let mut modem = with_sim(Rat::Lte, 4);
        // requestSetupDataCall, then requestOrSendDataCallList for cid 1.
        let setup =
            ["AT+CGDCONT=1,\"IPV4V6\",\"phone\",,0,0", "AT+CGQREQ=1", "AT+CGQMIN=1", "AT+CGEREP=1,0", "AT+CGACT=1,0", "ATD*99***1#"];
        for command in setup {
            assert_eq!(modem.answer(command), OK, "{command}");
        }
        let active = lines(&modem.answer("AT+CGACT?")).iter().map(|l| tokens(l)).collect::<Vec<_>>();
        assert_eq!(active, [["1", "1"]]);
        let contexts = lines(&modem.answer("AT+CGDCONT?")).iter().map(|l| tokens(l)).collect::<Vec<_>>();
        assert_eq!(contexts, [["1", "IPV4V6", "phone", "10.0.2.15/24", "0", "0"]]);
        let dynamic = tokens(&line(&modem.answer("AT+CGCONTRDP=1")));
        assert_eq!(dynamic, ["1", "5", "phone", "10.0.2.15/24", "10.0.2.2", "10.0.2.3"]);
        assert!(modem.answer("AT+CGCONTRDP=2").starts_with("+CME ERROR"));
        // A call the RIL starts while it still counts cid 1 as busy (one
        // Android dropped with the radio) gets cid 2: its line is the second.
        let mut modem = with_sim(Rat::Nr, 3);
        assert_eq!(modem.answer("AT+CGDCONT=2,\"IP\",\"internet\",,0,0"), OK);
        let active = lines(&modem.answer("AT+CGACT?")).iter().map(|l| tokens(l)).collect::<Vec<_>>();
        assert_eq!(active, [["1", "0"], ["2", "1"]]);
        assert_eq!(tokens(&line(&modem.answer("AT+CGDCONT?")))[0], "2");
        assert!(modem.answer("AT+CGDCONT=12,\"IP\",\"internet\",,0,0").starts_with("+CME ERROR"), "past MAX_PDP");
    }

    /// convertUsimToSim: the SIM response Android gets for a GET RESPONSE,
    /// as (file type, structure, size, record length).
    fn sim_response(fcp: &[u8]) -> (u8, u8, usize, u8) {
        assert_eq!(fcp[0], 0x62);
        assert_eq!(fcp[1] as usize, fcp.len() - 2);
        let descriptor = fcp.iter().position(|b| *b == 0x82).unwrap();
        let mut size = descriptor;
        while fcp[size] != 0x80 {
            size += fcp[size + 1] as usize + 2;
        }
        let size_bytes = (fcp[size + 2] as usize) << 8 | fcp[size + 3] as usize;
        match fcp[descriptor + 2] & 0x07 {
            0x01 => (4, 0, size_bytes, 0),
            0x02 => {
                assert_eq!((fcp[2], fcp[3]), (0x82, 5), "the RIL reads a linear file's descriptor from byte 2");
                (4, 1, size_bytes, ((fcp[6] as u16) << 8 | fcp[7] as u16) as u8)
            }
            other => panic!("structure {other}"),
        }
    }

    fn sim_data(answer: &str) -> (String, String, Vec<u8>) {
        let fields = tokens(&line(answer));
        let hex = fields.get(2).cloned().unwrap_or_default();
        let data = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        (fields[0].clone(), fields[1].clone(), data)
    }

    #[test]
    fn the_sim_has_the_files_android_reads() {
        let mut modem = with_sim(Rat::Lte, 4);
        let mut read = |file: u16| {
            // IccFileHandler: GET RESPONSE with 15 bytes, then the whole file.
            let (sw1, sw2, fcp) = sim_data(&modem.answer(&format!("AT+CRSM=192,{file},0,0,15")));
            assert_eq!((sw1.as_str(), sw2.as_str()), ("144", "0"));
            let (kind, structure, size, record) = sim_response(&fcp);
            assert_eq!(kind, 4, "an EF");
            let command = match structure {
                0 => format!("AT+CRSM=176,{file},0,0,{size}"),
                _ => format!("AT+CRSM=178,{file},1,4,{record}"),
            };
            let (sw1, _, data) = sim_data(&modem.answer(&command));
            assert_eq!(sw1, "144");
            assert_eq!(data.len(), if structure == 0 { size } else { record as usize });
            data
        };
        // EF_ICCID: IccUtils.bcdToString.
        let iccid: String = read(0x2FE2).iter().flat_map(|b| [b & 0xF, b >> 4]).map(|d| char::from(b'0' + d)).collect();
        assert_eq!(iccid, ICCID);
        // EF_AD: the MNC's length in the fourth byte.
        assert_eq!(read(0x6FAD)[3] & 0xF, 2);
        // EF_SPN: condition 0 (the name, on the home network), name until FF.
        let spn = read(0x6F46);
        assert_eq!(spn[0], 0);
        assert_eq!(spn[1..].iter().take_while(|b| **b != 0xFF).map(|b| *b as char).collect::<String>(), "Avo");
        // EF_MSISDN: AdnRecord, the 14-byte footer after the alpha tag.
        let msisdn = read(0x6F40);
        let footer = &msisdn[msisdn.len() - 14..];
        assert_eq!(footer[1], 0x91, "international");
        let digits = footer[2..1 + footer[0] as usize].iter().flat_map(|b| [b & 0xF, b >> 4]).filter(|d| *d < 10);
        let digits: String = digits.map(|d| char::from(b'0' + d)).collect();
        assert_eq!(format!("+{digits}"), MSISDN);
        // The rest, EF_IMSI's id (the ISIM's EF_IST) among them: not found.
        for file in [0x6F07, 0x6F38, 0x2F05, 0x6FC9] {
            assert_eq!(line(&modem.answer(&format!("AT+CRSM=192,{file},0,0,15"))), "+CRSM: 106,130");
        }
        assert_eq!(line(&modem.answer("AT+CRSM=178,28480,2,4,28")), "+CRSM: 106,131", "one MSISDN record");
        assert_eq!(line(&modem.answer("AT+CRSM=220,28480,1,4,28,FFFF")), "+CRSM: 144,0");
    }

    #[test]
    fn changes_reach_the_ril_as_unsolicited_lines() {
        let lte = Some(Service { rat: Rat::Lte, bars: 4 });
        let mut modem = Modem::default();
        modem.answer("AT+CREG=2");
        assert_eq!(modem.tell(lte), "", "the radio is off");
        assert_eq!(modem.service, lte);
        modem.answer("AT+CFUN=1");
        // Bars: the signal alone.
        let fewer = Some(Service { rat: Rat::Lte, bars: 1 });
        let told = modem.tell(fewer);
        assert_eq!(told.lines().count(), 1);
        assert_eq!(android_level(told.trim_end()), (Some("lte"), 1));
        assert_eq!(modem.tell(fewer), "", "nothing new");
        // Another network type: a registration line in the answer's form too.
        let told = modem.tell(Some(Service { rat: Rat::Edge, bars: 1 }));
        let told: Vec<&str> = told.lines().collect();
        assert_eq!(told.len(), 2);
        assert_eq!(tokens(told[0]), ["2", "1", "2142", "0000B804", "3"]);
        assert!(told[0].starts_with("+CREG:") && told[1].starts_with("+CSQ:"));
        // The SIM out, then in: the radio goes off for the RIL to ask again.
        assert_eq!(modem.tell(None), "+CFUN: 0\r\n");
        assert!(!modem.powered);
        assert_eq!(modem.answer("AT+CPIN?"), "+CME ERROR: 10\r\n");
        assert_eq!(modem.tell(lte), "", "off until Android turns it on");
        modem.answer("AT+CFUN=1");
        assert_eq!(line(&modem.answer("AT+CPIN?")), "+CPIN: READY");
        assert_eq!(modem.tell(None), "+CFUN: 0\r\n");
    }

    #[test]
    fn a_connection_hears_of_the_changes() {
        let cellular: &'static Cellular = Box::leak(Box::default());
        let (ours, theirs) = UnixStream::pair().unwrap();
        let connection = std::thread::spawn(move || session(ours, cellular));
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(theirs.try_clone().unwrap());
        let mut writer = theirs;
        let mut next = || {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line.trim_end().to_string()
        };
        for command in ["AT+CREG=2", "AT+CFUN=1"] {
            writer.write_all(format!("{command}\r").as_bytes()).unwrap();
            assert_eq!(next(), "OK");
        }
        cellular.set(Some(Service { rat: Rat::Hspa, bars: 3 }));
        assert_eq!(next(), "+CFUN: 0");
        writer.write_all(b"AT+CFUN=1\rAT+CPIN?\r").unwrap();
        assert_eq!([next(), next(), next()], ["OK", "+CPIN: READY", "OK"]);
        cellular.set(Some(Service { rat: Rat::Nr, bars: 4 }));
        assert_eq!(tokens(&next()), ["2", "1", "2142", "0000B804", "11"]);
        assert_eq!(android_level(&next()), (Some("nr"), 4));
        // A new connection (the RIL starting again) starts with the SIM in.
        writer.shutdown(Shutdown::Both).unwrap();
        connection.join().unwrap();
        assert!(cellular.state.lock().unwrap().sessions.is_empty());
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || session(ours, cellular));
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        theirs.write_all(b"AT+CPIN?\r").unwrap();
        let mut answer = [0u8; 18];
        std::io::Read::read_exact(&mut theirs, &mut answer).unwrap();
        assert_eq!(&answer, b"+CPIN: READY\r\nOK\r\n");
    }
}
