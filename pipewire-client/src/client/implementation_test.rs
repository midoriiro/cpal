use crate::client::implementation::CLIENT_INDEX;
use crate::constants::*;
use crate::PipewireClient;
use docker_api::models::ImageBuildChunk;
use docker_api::opts::ImageBuildOpts;
use docker_api::Docker;
use futures::StreamExt;
use pipewire::spa::utils::dict::ParsableValue;
use rstest::{fixture, rstest};
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use testcontainers::core::{CmdWaitFor, ExecCommand, Mount};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, Image, ImageExt};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

struct Container {
    name: String,
    tag: String,
    container_file_path: PathBuf,
    container: Option<ContainerAsync<GenericImage>>,
    socket_id: Uuid,
    pipewire_pid: Option<u32>,
    wireplumber_pid: Option<u32>,
    pulse_pid: Option<u32>,
}

impl Container {
    pub fn new(
        name: String,
        container_file_path: PathBuf,
    ) -> Self {
        Self {
            name,
            tag: "latest".to_string(),
            container_file_path,
            container: None,
            socket_id: Uuid::new_v4(),
            pipewire_pid: None,
            wireplumber_pid: None,
            pulse_pid: None,
        }
    }

    pub fn socket_location(&self) -> PathBuf {
        Path::new("/run/pipewire-sockets").join(self.socket_id.to_string()).to_path_buf()
    }

    pub fn socket_name(&self) -> String {
        format!("{}", self.socket_id)
    }

    pub fn build(&self) {
        const DOCKER_HOST_ENVIRONMENT_KEY: &str = "DOCKER_HOST";
        const CONTAINER_HOST_ENVIRONMENT_KEY: &str = "CONTAINER_HOST";
        let docker_host = std::env::var(DOCKER_HOST_ENVIRONMENT_KEY);
        let container_host = std::env::var(CONTAINER_HOST_ENVIRONMENT_KEY);
        let uri = match (docker_host, container_host) {
            (Ok(value), Ok(_)) => value,
            (Ok(value), Err(_)) => value,
            (Err(_), Ok(value)) => {
                // TestContainer does not recognize CONTAINER_HOST.
                // Instead, with set DOCKET_HOST env var with the same value
                std::env::set_var(DOCKER_HOST_ENVIRONMENT_KEY, value.clone());
                value
            },
            (Err(_), Err(_)) => panic!(
                "${} or ${} should be set.",
                DOCKER_HOST_ENVIRONMENT_KEY, CONTAINER_HOST_ENVIRONMENT_KEY
            ),
        };
        let api = Docker::new(uri).unwrap();
        let images = api.images();
        let build_image_options= ImageBuildOpts::builder(self.container_file_path.parent().unwrap().to_str().unwrap())
            .tag(format!("{}:{}", self.name, self.tag))
            .dockerfile(self.container_file_path.file_name().unwrap().to_str().unwrap())
            .build();
        let mut stream = images.build(&build_image_options);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        while let Some(build_result) = runtime.block_on(stream.next()) {
            match build_result {
                Ok(output) => {
                    let output = match output {
                        ImageBuildChunk::Update { stream } => stream,
                        ImageBuildChunk::Error { error, error_detail } => {
                            panic!("Error {}: {}", error, error_detail.message);
                        }
                        ImageBuildChunk::Digest { aux } => aux.id,
                        ImageBuildChunk::PullStatus { .. } => {
                            return
                        }
                    };
                    print!("{}", output);
                },
                Err(e) => panic!("Error: {e}"),
            }
        }
    }

    pub fn run(&mut self) {
        let socket_location = self.socket_location();
        let socket_name = self.socket_name();
        let container = GenericImage::new(self.name.clone(), self.tag.clone())
            .with_env_var(PIPEWIRE_RUNTIME_DIR_ENVIRONMENT_KEY, socket_location.to_str().unwrap())
            .with_env_var(PIPEWIRE_CORE_ENVIRONMENT_KEY, socket_name.clone())
            .with_env_var(PIPEWIRE_REMOTE_ENVIRONMENT_KEY, socket_name.clone())
            .with_env_var("PULSE_RUNTIME_PATH", socket_location.join("pulse").to_str().unwrap())
            .with_mount(Mount::volume_mount(
                "pipewire-sockets",
                socket_location.parent().unwrap().to_str().unwrap(),
            ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let container = runtime.block_on(container.start()).unwrap();
        self.container = Some(container);
        runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                "mkdir",
                "--parent",
                socket_location.to_str().unwrap(),
            ])
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
        self.start_pipewire();
    }

    fn run_process(&mut self, process_name: &str) -> u32 {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                process_name
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
        let mut result = runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                "pidof",
                process_name,
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
        let mut pid = String::new();
        runtime.block_on(result.stdout().read_to_string(&mut pid)).unwrap();
        pid = pid.trim_end().to_string();
        u32::parse_value(pid.as_str()).unwrap()
    }

    fn kill_process(&mut self, process_id: u32) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                "kill",
                format!("{}", process_id).as_str()
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
    }

    fn start_pipewire(&mut self) {
        let pid = self.run_process("pipewire");
        self.pipewire_pid = Some(pid);
    }

    fn stop_pipewire(&mut self) {
        self.kill_process(self.pipewire_pid.unwrap())
    }

    pub fn start_wireplumber(&mut self) {
        let pid = self.run_process("wireplumber");
        self.wireplumber_pid = Some(pid);
    }

    pub fn stop_wireplumber(&mut self) {
        if self.wireplumber_pid.is_none() {
            return;
        }
        self.kill_process(self.wireplumber_pid.unwrap());
    }

    pub fn start_pulse(&mut self) {
        let pid = self.run_process("pipewire-pulse");
        self.pulse_pid = Some(pid);
    }

    pub fn stop_pulse(&mut self) {
        if self.pulse_pid.is_none() {
            return;
        }
        self.kill_process(self.pulse_pid.unwrap());
    }

    pub fn load_null_sink_module(&self) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                "pactl",
                "load-module",
                "module-null-sink"
            ])
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
    }

    pub fn create_virtual_node(&self) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(self.container.as_ref().unwrap().exec(
            ExecCommand::new(vec![
                "pw-cli",
                "create-node",
                "adapter",
                "'{ factory.name=support.null-audio-sink node.name=test-sink media.class=Audio/Sink object.linger=true audio.position=[FL FR] }'"
            ])
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.container.is_none() {
            return;
        }
        self.stop_pulse();
        self.stop_wireplumber();
        self.stop_pipewire();
        let socket_location = self.socket_location();
        let container = self.container.take().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(container.exec(
            ExecCommand::new(vec![
                "rm",
                "--force",
                "--recursive",
                socket_location.join("*").to_str().unwrap(),
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )).unwrap();
        runtime.block_on(container.stop()).unwrap();
        runtime.block_on(container.rm()).unwrap();
    }
}

#[fixture]
fn pipewire_server_with_default_configuration() -> Container {
    let mut container = Container::new(
        "pipewire-default".to_string(),
        PathBuf::from(".containers/pipewire.test.container"),
    );
    container.build();
    container.run();
    container.start_wireplumber();
    container.start_pulse();
    container.load_null_sink_module();
    container.create_virtual_node();
    container
}

#[fixture]
fn pipewire_server_without_session_manager() -> Container {
    let mut container = Container::new(
        "pipewire-without-session-manager".to_string(),
        PathBuf::from(".containers/pipewire.test.container"),
    );
    container.build();
    container.run();
    container
}

#[fixture]
fn pipewire_server_without_node() -> Container {
    let mut container = Container::new(
        "pipewire-without-node".to_string(),
        PathBuf::from(".containers/pipewire.test.container"),
    );
    container.build();
    container.run();
    container.start_wireplumber();
    container
}

fn set_socket_env_vars(server: &Container) {
    std::env::set_var(PIPEWIRE_RUNTIME_DIR_ENVIRONMENT_KEY, server.socket_location());
    std::env::set_var(PIPEWIRE_REMOTE_ENVIRONMENT_KEY, server.socket_name());
}

#[rstest]
#[serial]
pub fn initialization() {
    let _ = PipewireClient::new().unwrap();
}

#[rstest]
#[serial]
pub fn name() {
    let client_1 = PipewireClient::new().unwrap();
    assert_eq!(format!("cpal-client-{}", CLIENT_INDEX.load(Ordering::SeqCst) - 1), client_1.name);
    let client_2 = PipewireClient::new().unwrap();
    assert_eq!(format!("cpal-client-{}", CLIENT_INDEX.load(Ordering::SeqCst) - 1), client_2.name);
}

#[rstest]
#[serial]
pub fn server_with_default_configuration(pipewire_server_with_default_configuration: Container) {
    set_socket_env_vars(&pipewire_server_with_default_configuration);
    let _ = PipewireClient::new().unwrap();
}

#[rstest]
#[serial]
pub fn server_without_session_manager(pipewire_server_without_session_manager: Container) {
    set_socket_env_vars(&pipewire_server_without_session_manager);
    let error = PipewireClient::new().unwrap_err();
    assert_eq!(true, error.description.contains("No session manager registered"))
}

#[rstest]
#[serial]
pub fn server_without_node(pipewire_server_without_node: Container) {
    set_socket_env_vars(&pipewire_server_without_node);
    let error = PipewireClient::new().unwrap_err();
    assert_eq!("Post initialization error: Zero node registered", error.description)
}