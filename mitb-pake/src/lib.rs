use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use opaque_ke::argon2::Argon2;
use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::errors::ProtocolError;
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialFinalization, CredentialRequest,
    CredentialResponse, RegistrationRequest, RegistrationResponse, RegistrationUpload, ServerLogin,
    ServerLoginParameters, ServerRegistration, ServerSetup,
};
use rand_core::{CryptoRng, RngCore};
use sha2::Sha512;
use std::fmt::Display;

struct DefaultCipherSuite;

impl CipherSuite for DefaultCipherSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, Sha512>;
    type Ksf = Argon2<'static>;
}

fn error_string(error: impl Display) -> String {
    error.to_string()
}

struct RandomSource;

impl RngCore for RandomSource {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0_u8; 4];
        if self.try_fill_bytes(&mut bytes).is_err() {
            bytes.fill(0);
        }
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0_u8; 8];
        if self.try_fill_bytes(&mut bytes).is_err() {
            bytes.fill(0);
        }
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if self.try_fill_bytes(dest).is_err() {
            dest.fill(0);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        fill_random(dest).map_err(|error| rand_core::Error::from(error.code()))
    }
}

impl CryptoRng for RandomSource {}

fn new_rng() -> RandomSource {
    RandomSource
}

fn fill_random(dest: &mut [u8]) -> Result<(), getrandom::Error> {
    getrandom::getrandom(dest)
}

#[cfg(target_arch = "wasm32")]
fn browser_getrandom(dest: &mut [u8]) -> Result<(), getrandom::Error> {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn mitb_fill_random(ptr: u32, len: u32) -> u32;
    }

    let status = unsafe { mitb_fill_random(dest.as_mut_ptr() as u32, dest.len() as u32) };
    if status == 0 {
        Ok(())
    } else {
        let code = std::num::NonZeroU32::new(getrandom::Error::CUSTOM_START + 1)
            .unwrap_or(std::num::NonZeroU32::MIN);
        Err(getrandom::Error::from(code))
    }
}

#[cfg(target_arch = "wasm32")]
getrandom::register_custom_getrandom!(browser_getrandom);

pub fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode(encoded: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD.decode(encoded).map_err(error_string)
}

#[derive(Debug)]
pub struct ClientLoginStart {
    pub credential_request: Vec<u8>,
}

#[derive(Debug)]
pub struct ClientRegistrationStart {
    pub registration_request: Vec<u8>,
}

#[derive(Debug)]
pub struct ClientRegistrationFinish {
    pub registration_upload: Vec<u8>,
    pub credential_request: Vec<u8>,
}

#[derive(Debug)]
pub struct ClientLoginFinish {
    pub credential_finalization: Vec<u8>,
}

pub struct ClientSession {
    secret_code: Vec<u8>,
    registration_state: Option<ClientRegistration<DefaultCipherSuite>>,
    login_state: Option<Vec<u8>>,
}

impl ClientSession {
    pub fn new(secret_code: &str) -> Result<Self, String> {
        let secret_code = secret_code.trim();
        if secret_code.is_empty() {
            return Err(String::from("missing secret code"));
        }

        Ok(Self {
            secret_code: secret_code.as_bytes().to_vec(),
            registration_state: None,
            login_state: None,
        })
    }

    pub fn start_login(&mut self) -> Result<ClientLoginStart, String> {
        let mut rng = new_rng();
        let login = ClientLogin::<DefaultCipherSuite>::start(&mut rng, &self.secret_code)
            .map_err(error_string)?;
        let credential_request = login.message.serialize().to_vec();
        self.login_state = Some(login.state.serialize().to_vec());

        Ok(ClientLoginStart { credential_request })
    }

    pub fn start_registration(&mut self) -> Result<ClientRegistrationStart, String> {
        let mut rng = new_rng();
        let registration =
            ClientRegistration::<DefaultCipherSuite>::start(&mut rng, &self.secret_code)
                .map_err(error_string)?;
        let registration_request = registration.message.serialize().to_vec();
        self.registration_state = Some(registration.state);

        Ok(ClientRegistrationStart {
            registration_request,
        })
    }

    pub fn finish_registration(
        &mut self,
        registration_response_bytes: &[u8],
    ) -> Result<ClientRegistrationFinish, String> {
        let registration_state = self
            .registration_state
            .take()
            .ok_or_else(|| String::from("registration was not started"))?;
        let registration_response =
            RegistrationResponse::deserialize(registration_response_bytes).map_err(error_string)?;
        let mut rng = new_rng();
        let upload = registration_state
            .finish(
                &mut rng,
                &self.secret_code,
                registration_response,
                ClientRegistrationFinishParameters::default(),
            )
            .map_err(error_string)?;
        let login = self.start_login()?;

        Ok(ClientRegistrationFinish {
            registration_upload: upload.message.serialize().to_vec(),
            credential_request: login.credential_request,
        })
    }

    pub fn finish_login(
        &mut self,
        credential_response_bytes: &[u8],
    ) -> Result<ClientLoginFinish, String> {
        let login_state_bytes = self
            .login_state
            .as_ref()
            .ok_or_else(|| String::from("login was not started"))?;
        let login_state = ClientLogin::<DefaultCipherSuite>::deserialize(login_state_bytes)
            .map_err(login_error_string)?;
        let credential_response =
            CredentialResponse::deserialize(credential_response_bytes).map_err(error_string)?;
        let mut rng = new_rng();
        let finish = login_state
            .finish(
                &mut rng,
                &self.secret_code,
                credential_response,
                ClientLoginFinishParameters::default(),
            )
            .map_err(login_error_string)?;
        self.login_state = None;

        Ok(ClientLoginFinish {
            credential_finalization: finish.message.serialize().to_vec(),
        })
    }
}

pub struct ServerPake {
    server_setup: ServerSetup<DefaultCipherSuite>,
}

impl ServerPake {
    pub fn new() -> Self {
        let mut rng = new_rng();
        Self {
            server_setup: ServerSetup::<DefaultCipherSuite>::new(&mut rng),
        }
    }

    pub fn registration_response(
        &self,
        credential_identifier: &[u8],
        registration_request_bytes: &[u8],
    ) -> Result<Vec<u8>, String> {
        let registration_request =
            RegistrationRequest::deserialize(registration_request_bytes).map_err(error_string)?;
        let response = ServerRegistration::<DefaultCipherSuite>::start(
            &self.server_setup,
            registration_request,
            credential_identifier,
        )
        .map_err(error_string)?;

        Ok(response.message.serialize().to_vec())
    }

    pub fn finish_registration(&self, registration_upload_bytes: &[u8]) -> Result<Vec<u8>, String> {
        let registration_upload =
            RegistrationUpload::<DefaultCipherSuite>::deserialize(registration_upload_bytes)
                .map_err(error_string)?;
        let password_file = ServerRegistration::finish(registration_upload);
        Ok(password_file.serialize().to_vec())
    }

    pub fn start_login(
        &self,
        credential_identifier: &[u8],
        password_file_bytes: &[u8],
        credential_request_bytes: &[u8],
    ) -> Result<(Vec<u8>, ServerLoginSession), String> {
        let password_file =
            ServerRegistration::<DefaultCipherSuite>::deserialize(password_file_bytes)
                .map_err(error_string)?;
        let credential_request =
            CredentialRequest::deserialize(credential_request_bytes).map_err(error_string)?;
        let mut rng = new_rng();
        let login = ServerLogin::start(
            &mut rng,
            &self.server_setup,
            Some(password_file),
            credential_request,
            credential_identifier,
            ServerLoginParameters::default(),
        )
        .map_err(error_string)?;

        Ok((
            login.message.serialize().to_vec(),
            ServerLoginSession { state: login.state },
        ))
    }
}

fn login_error_string(error: ProtocolError) -> String {
    match error {
        ProtocolError::InvalidLoginError => String::from("invalid secret code"),
        other => other.to_string(),
    }
}

impl Default for ServerPake {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ServerLoginSession {
    state: ServerLogin<DefaultCipherSuite>,
}

impl ServerLoginSession {
    pub fn finish(self, credential_finalization_bytes: &[u8]) -> Result<(), String> {
        let credential_finalization =
            CredentialFinalization::deserialize(credential_finalization_bytes)
                .map_err(error_string)?;
        self.state
            .finish(credential_finalization, ServerLoginParameters::default())
            .map(|_| ())
            .map_err(error_string)
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_bridge {
    use super::{ClientSession, decode, encode};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::slice;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    static SESSIONS: OnceLock<Mutex<HashMap<u32, ClientSession>>> = OnceLock::new();
    static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
    static LAST_RESULT_LEN: AtomicU32 = AtomicU32::new(0);

    fn sessions() -> &'static Mutex<HashMap<u32, ClientSession>> {
        SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Request {
        Create {
            secret_code: String,
        },
        Destroy {
            handle: u32,
        },
        StartLogin {
            handle: u32,
        },
        StartRegistration {
            handle: u32,
        },
        FinishRegistration {
            handle: u32,
            registration_response: String,
        },
        FinishLogin {
            handle: u32,
            credential_response: String,
        },
    }

    #[derive(Default, Serialize)]
    struct Response {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        handle: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credential_request: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        registration_request: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        registration_upload: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credential_finalization: Option<String>,
    }

    fn serialize_response(response: Response) -> u32 {
        let bytes = serde_json::to_vec(&response)
            .unwrap_or_else(|error| format!("{{\"ok\":false,\"error\":{error:?}}}").into_bytes());
        let mut boxed = bytes.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        LAST_RESULT_LEN.store(boxed.len() as u32, Ordering::Relaxed);
        std::mem::forget(boxed);
        ptr as u32
    }

    fn ok_response() -> Response {
        Response {
            ok: true,
            ..Response::default()
        }
    }

    fn error_response(error: String) -> Response {
        Response {
            ok: false,
            error: Some(error),
            ..Response::default()
        }
    }

    fn process_request(request: Request) -> Result<Response, String> {
        match request {
            Request::Create { secret_code } => {
                let session = ClientSession::new(&secret_code)?;
                let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?
                    .insert(handle, session);

                Ok(Response {
                    ok: true,
                    handle: Some(handle),
                    ..Response::default()
                })
            }
            Request::Destroy { handle } => {
                sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?
                    .remove(&handle);
                Ok(ok_response())
            }
            Request::StartLogin { handle } => {
                let mut guard = sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?;
                let session = guard
                    .get_mut(&handle)
                    .ok_or_else(|| String::from("unknown session handle"))?;
                let login = session.start_login()?;

                Ok(Response {
                    ok: true,
                    credential_request: Some(encode(&login.credential_request)),
                    ..Response::default()
                })
            }
            Request::StartRegistration { handle } => {
                let mut guard = sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?;
                let session = guard
                    .get_mut(&handle)
                    .ok_or_else(|| String::from("unknown session handle"))?;
                let registration = session.start_registration()?;

                Ok(Response {
                    ok: true,
                    registration_request: Some(encode(&registration.registration_request)),
                    ..Response::default()
                })
            }
            Request::FinishRegistration {
                handle,
                registration_response,
            } => {
                let response = decode(&registration_response)?;
                let mut guard = sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?;
                let session = guard
                    .get_mut(&handle)
                    .ok_or_else(|| String::from("unknown session handle"))?;
                let registration = session.finish_registration(&response)?;

                Ok(Response {
                    ok: true,
                    registration_upload: Some(encode(&registration.registration_upload)),
                    credential_request: Some(encode(&registration.credential_request)),
                    ..Response::default()
                })
            }
            Request::FinishLogin {
                handle,
                credential_response,
            } => {
                let response = decode(&credential_response)?;
                let mut guard = sessions()
                    .lock()
                    .map_err(|_| String::from("failed to lock sessions"))?;
                let session = guard
                    .get_mut(&handle)
                    .ok_or_else(|| String::from("unknown session handle"))?;
                let login = session.finish_login(&response)?;

                Ok(Response {
                    ok: true,
                    credential_finalization: Some(encode(&login.credential_finalization)),
                    ..Response::default()
                })
            }
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn mitb_pake_alloc(len: u32) -> u32 {
        let mut buffer = Vec::<u8>::with_capacity(len as usize);
        let ptr = buffer.as_mut_ptr();
        std::mem::forget(buffer);
        ptr as u32
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn mitb_pake_free(ptr: u32, len: u32) {
        if ptr == 0 || len == 0 {
            return;
        }

        unsafe {
            let _ = Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize);
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn mitb_pake_last_result_len() -> u32 {
        LAST_RESULT_LEN.load(Ordering::Relaxed)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn mitb_pake_process_json(ptr: u32, len: u32) -> u32 {
        let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, len as usize) };
        let response = match serde_json::from_slice::<Request>(bytes) {
            Ok(request) => match process_request(request) {
                Ok(response) => response,
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error.to_string()),
        };

        serialize_response(response)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientSession, ServerPake};

    #[test]
    fn registration_then_login_round_trip() -> Result<(), String> {
        let secret = "my-shared-code";
        let mut client = ClientSession::new(secret)?;
        let server = ServerPake::new();
        let credential_identifier = b"room-handle";

        client.start_login()?;

        let registration = client.start_registration()?;
        let registration_response = server
            .registration_response(credential_identifier, &registration.registration_request)?;
        let registration_finish = client.finish_registration(&registration_response)?;
        let password_file = server.finish_registration(&registration_finish.registration_upload)?;

        let (credential_response, login_session) = server.start_login(
            credential_identifier,
            &password_file,
            &registration_finish.credential_request,
        )?;
        let login_finish = client.finish_login(&credential_response)?;
        login_session.finish(&login_finish.credential_finalization)?;
        Ok(())
    }

    #[test]
    fn login_state_can_try_multiple_candidates() -> Result<(), String> {
        let mut client = ClientSession::new("my-shared-code")?;
        let mut wrong_client = ClientSession::new("other-secret")?;
        let server = ServerPake::new();
        let wrong_identifier = b"room-a";
        let correct_identifier = b"room-b";

        let wrong_registration = wrong_client.start_registration()?;
        let wrong_response = server
            .registration_response(wrong_identifier, &wrong_registration.registration_request)?;
        let wrong_finish = wrong_client.finish_registration(&wrong_response)?;
        let wrong_password_file = server.finish_registration(&wrong_finish.registration_upload)?;

        let correct_registration = client.start_registration()?;
        let correct_response = server.registration_response(
            correct_identifier,
            &correct_registration.registration_request,
        )?;
        let correct_finish = client.finish_registration(&correct_response)?;
        let correct_password_file =
            server.finish_registration(&correct_finish.registration_upload)?;

        let login = client.start_login()?;
        let (wrong_credential_response, _) = server.start_login(
            wrong_identifier,
            &wrong_password_file,
            &login.credential_request,
        )?;
        let wrong_login = client.finish_login(&wrong_credential_response);
        assert!(matches!(
            wrong_login,
            Err(ref error) if error == "invalid secret code"
        ));

        let (correct_credential_response, login_session) = server.start_login(
            correct_identifier,
            &correct_password_file,
            &login.credential_request,
        )?;
        let login_finish = client.finish_login(&correct_credential_response)?;
        login_session.finish(&login_finish.credential_finalization)?;
        Ok(())
    }
}
