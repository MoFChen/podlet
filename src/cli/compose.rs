use std::{
    collections::HashMap,
    fs,
    io::{self, IsTerminal},
    mem,
    path::{Path, PathBuf},
};

use clap::Args;
use color_eyre::{
    eyre::{bail, ensure, eyre, OptionExt, WrapErr},
    Help,
};
use compose_spec::{service::Command, Identifier, Network, Networks, Resource, Service, Volumes};
use indexmap::IndexMap;

use crate::quadlet::{self, container::volume::Source, Globals};

use super::{k8s, Container, File, GlobalArgs, Unit};

/// Converts a [`Command`] into a [`Vec<String>`], splitting the [`String`](Command::String) variant
/// as a shell would.
///
/// # Errors
///
/// Returns an error if, while splitting the string variant, the command ends while in a quote or
/// has a trailing unescaped '\\'.
pub fn command_try_into_vec(command: Command) -> color_eyre::Result<Vec<String>> {
    match command {
        Command::String(command) => shlex::split(&command)
            .ok_or_else(|| eyre!("invalid command: `{command}`"))
            .suggestion(
                "In the command, make sure quotes are closed properly and there are no \
                    trailing \\. Alternatively, use an array instead of a string.",
            ),
        Command::List(command) => Ok(command),
    }
}

/// [`Args`] for the `podlet compose` subcommand.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct Compose {
    /// Create a `.pod` file and link it with each `.container` file.
    ///
    /// The top-level `name` field in the compose file is required when using this option.
    /// It is used for the name of the pod and in the filenames of the created files.
    ///
    /// Each container becomes a part of the pod and is renamed to "{pod}-{container}".
    ///
    /// Published ports are taken from each container and applied to the pod.
    #[arg(long, conflicts_with = "kube")]
    pub pod: bool,

    /// Create a Kubernetes YAML file for a pod instead of separate containers
    ///
    /// A `.kube` file using the generated Kubernetes YAML file is also created.
    ///
    /// The top-level `name` field in the compose file is required when using this option.
    /// It is used for the name of the pod and in the filenames of the created files.
    #[arg(long, conflicts_with = "pod")]
    pub kube: bool,

    /// The compose file to convert
    ///
    /// If `-` or not provided and stdin is not a terminal,
    /// the compose file will be read from stdin.
    ///
    /// If not provided, and stdin is a terminal, Podlet will look for (in order)
    /// `compose.yaml`, `compose.yml`, `docker-compose.yaml`, and `docker-compose.yml`,
    /// in the current working directory.
    #[allow(clippy::struct_field_names)]
    pub compose_file: Option<PathBuf>,
}

impl Compose {
    /// Attempt to convert the `compose_file` into [`File`]s.
    ///
    /// # Errors
    ///
    /// Returns an error if there was an error:
    ///
    /// - Reading/deserializing the compose file.
    /// - Converting the compose file to Kubernetes YAML.
    /// - Converting the compose file to Quadlet files.
    pub fn try_into_files(
        self,
        unit: Option<Unit>,
        install: Option<quadlet::Install>,
    ) -> color_eyre::Result<Vec<File>> {
        let Self {
            pod,
            kube,
            compose_file,
        } = self;

        let compose = read_from_file_or_stdin(compose_file.as_deref())
            .wrap_err("error reading compose file")?;

        if kube {
            let mut k8s_file = k8s::File::try_from(compose)
                .wrap_err("error converting compose file into Kubernetes YAML")?;

            let kube =
                quadlet::Kube::new(PathBuf::from(format!("{}-kube.yaml", k8s_file.name)).into());
            let quadlet_file = quadlet::File {
                name: k8s_file.name.clone(),
                unit,
                resource: kube.into(),
                globals: Globals::default(),
                service: None,
                install,
            };

            k8s_file.name.push_str("-kube");
            Ok(vec![quadlet_file.into(), k8s_file.into()])
        } else {
            let compose_spec::Compose {
                version: _,
                name,
                include,
                services,
                networks,
                volumes,
                configs,
                secrets,
                extensions,
            } = compose;

            let pod_name = pod
                .then(|| name.ok_or_eyre("`name` is required when using `--pod`"))
                .transpose()?
                .map(Into::into);

            ensure!(include.is_empty(), "`include` is not supported");
            ensure!(configs.is_empty(), "`configs` is not supported");
            ensure!(
                secrets.values().all(Resource::is_external),
                "only external `secrets` are supported",
            );
            ensure!(
                extensions.is_empty(),
                "compose extensions are not supported"
            );

            parts_try_into_files(services, networks, volumes, pod_name, unit, install)
                .wrap_err("error converting compose file into Quadlet files")
        }
    }
}

/// Read and deserialize a [`compose_spec::Compose`] from a file at the given [`Path`], stdin, or a
/// list of default files.
///
/// If the path is '-', or stdin is not a terminal, the compose file is deserialized from stdin.
/// If a path is not provided, the files `compose.yaml`, `compose.yml`, `docker-compose.yaml`,
/// and `docker-compose.yml` are, in order, looked for in the current directory.
///
/// # Errors
///
/// Returns an error if:
///
/// - There was an error opening the given file.
/// - Stdin was selected and stdin is a terminal.
/// - No path was given and none of the default files could be opened.
/// - There was an error deserializing [`compose_spec::Compose`].
fn read_from_file_or_stdin(path: Option<&Path>) -> color_eyre::Result<compose_spec::Compose> {
    let (compose_file, path) = if let Some(path) = path {
        if path.as_os_str() == "-" {
            return read_from_stdin();
        }
        let compose_file = fs::File::open(path)
            .wrap_err("could not open provided compose file")
            .suggestion("make sure you have the proper permissions for the given file")?;
        (compose_file, path)
    } else {
        const FILE_NAMES: [&str; 4] = [
            "compose.yaml",
            "compose.yml",
            "docker-compose.yaml",
            "docker-compose.yml",
        ];

        if !io::stdin().is_terminal() {
            return read_from_stdin();
        }

        let mut result = None;
        for file_name in FILE_NAMES {
            if let Ok(compose_file) = fs::File::open(file_name) {
                result = Some((compose_file, file_name.as_ref()));
                break;
            }
        }

        result.ok_or_eyre(
            "a compose file was not provided and none of \
                `compose.yaml`, `compose.yml`, `docker-compose.yaml`, or `docker-compose.yml` \
                exist in the current directory or could not be read",
        )?
    };

    serde_yaml::from_reader(compose_file)
        .wrap_err_with(|| format!("File `{}` is not a valid compose file", path.display()))
}

/// Read and deserialize [`compose_spec::Compose`] from stdin.
///
/// # Errors
///
/// Returns an error if stdin is a terminal or there was an error deserializing.
fn read_from_stdin() -> color_eyre::Result<compose_spec::Compose> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        bail!("cannot read compose from stdin, stdin is a terminal");
    }

    serde_yaml::from_reader(stdin).wrap_err("data from stdin is not a valid compose file")
}

/// Attempt to convert [`Service`]s, [`Networks`], and [`Volumes`] into [`File`]s.
///
/// # Errors
///
/// Returns an error if a [`Service`], [`Network`], or [`Volume`](compose_spec::Volume) could not be
/// converted into a [`quadlet::File`].
fn parts_try_into_files(
    services: IndexMap<Identifier, Service>,
    networks: Networks,
    volumes: Volumes,
    pod_name: Option<String>,
    unit: Option<Unit>,
    install: Option<quadlet::Install>,
) -> color_eyre::Result<Vec<File>> {
    // Get a map of volumes to whether the volume has options associated with it for use in
    // converting a service into a Quadlet file. Extra volume options must be specified in a
    // separate Quadlet file which is referenced from the container Quadlet file.
    let volume_has_options = volumes
        .iter()
        .map(|(name, volume)| {
            let has_options = volume
                .as_ref()
                .and_then(Resource::as_compose)
                .is_some_and(|volume| !volume.is_empty());
            (name.clone(), has_options)
        })
        .collect();

    let mut pod_ports = Vec::new();
    let mut files = services
        .into_iter()
        .map(|(name, service)| {
            let mut file = service_try_into_quadlet_file(
                service,
                name,
                unit.clone(),
                install.clone(),
                &volume_has_options,
            )?;
            if let (
                Some(pod_name),
                quadlet::File {
                    name,
                    resource: quadlet::Resource::Container(container),
                    ..
                },
            ) = (&pod_name, &mut file)
            {
                *name = format!("{pod_name}-{name}");
                pod_ports.extend(mem::take(&mut container.publish_port));
                container.pod = Some(format!("{pod_name}.pod"));
            }
            Ok(file)
        })
        .chain(networks_try_into_quadlet_files(
            networks,
            unit.as_ref(),
            install.as_ref(),
        ))
        .chain(volumes_try_into_quadlet_files(
            volumes,
            unit.as_ref(),
            install.as_ref(),
        ))
        .map(|result| result.map(Into::into))
        .collect::<Result<Vec<File>, _>>()?;

    if let Some(name) = pod_name {
        let pod = quadlet::Pod {
            publish_port: pod_ports,
            ..quadlet::Pod::default()
        };
        let pod = quadlet::File {
            name,
            unit,
            resource: pod.into(),
            globals: Globals::default(),
            service: None,
            install,
        };
        files.push(pod.into());
    }

    Ok(files)
}

/// Attempt to convert a compose [`Service`] into a [`quadlet::File`].
///
/// `volume_has_options` should be a map from volume [`Identifier`]s to whether the volume has any
/// options set. It is used to determine whether to link to a [`quadlet::Volume`] in the created
/// [`quadlet::Container`].
///
/// # Errors
///
/// Returns an error if there was an error [adding](Unit::add_dependency()) a service
/// [`Dependency`](compose_spec::service::Dependency) to the [`Unit`] or converting the [`Service`]
/// into a [`quadlet::Container`].
fn service_try_into_quadlet_file(
    mut service: Service,
    name: Identifier,
    mut unit: Option<Unit>,
    install: Option<quadlet::Install>,
    volume_has_options: &HashMap<Identifier, bool>,
) -> color_eyre::Result<quadlet::File> {
    // Add any service dependencies to the [Unit] section of the Quadlet file.
    let dependencies = mem::take(&mut service.depends_on).into_long();
    if !dependencies.is_empty() {
        let unit = unit.get_or_insert_with(Unit::default);
        for (ident, dependency) in dependencies {
            unit.add_dependency(&ident, dependency).wrap_err_with(|| {
                format!("error adding dependency on `{ident}` to service `{name}`")
            })?;
        }
    }

    let global_args = GlobalArgs::from_compose(&mut service);

    let restart = service.restart;

    let mut container = Container::try_from(service)
        .map(quadlet::Container::from)
        .wrap_err_with(|| format!("error converting service `{name}` into a Quadlet container"))?;

    // For each named volume, check to see if it has any options set.
    // If it does, add `.volume` to the source to link this `.container` file to the generated
    // `.volume` file.
    for volume in &mut container.volume {
        if let Some(Source::NamedVolume(source)) = &mut volume.source {
            let volume_has_options = volume_has_options
                .get(source.as_str())
                .copied()
                .unwrap_or_default();
            if volume_has_options {
                source.push_str(".volume");
            }
        }
    }

    Ok(quadlet::File {
        name: name.into(),
        unit,
        resource: container.into(),
        globals: global_args.into(),
        service: restart.map(Into::into),
        install,
    })
}

/// Attempt to convert compose [`Networks`] into an [`Iterator`] of [`quadlet::File`]s.
///
/// # Errors
///
/// The [`Iterator`] returns an [`Err`] if a [`Network`] could not be converted into a
/// [`quadlet::Network`].
fn networks_try_into_quadlet_files<'a>(
    networks: Networks,
    unit: Option<&'a Unit>,
    install: Option<&'a quadlet::Install>,
) -> impl Iterator<Item = color_eyre::Result<quadlet::File>> + 'a {
    networks.into_iter().map(move |(name, network)| {
        let network = match network {
            Some(Resource::Compose(network)) => network,
            None => Network::default(),
            Some(Resource::External { .. }) => {
                bail!("external networks (`{name}`) are not supported");
            }
        };
        let network = quadlet::Network::try_from(network).wrap_err_with(|| {
            format!("error converting network `{name}` into a Quadlet network")
        })?;

        Ok(quadlet::File {
            name: name.into(),
            unit: unit.cloned(),
            resource: network.into(),
            globals: Globals::default(),
            service: None,
            install: install.cloned(),
        })
    })
}

/// Attempt to convert compose [`Volumes`] into an [`Iterator`] of [`quadlet::File`]s.
///
/// [`Volume`](compose_spec::Volume)s which are [empty](compose_spec::Volume::is_empty()) are
/// filtered out as they do not need a `.volume` Quadlet file to define extra options.
///
/// # Errors
///
/// The [`Iterator`] returns an [`Err`] if a [`Volume`](compose_spec::Volume) could not be converted
/// to a [`quadlet::Volume`].
fn volumes_try_into_quadlet_files<'a>(
    volumes: Volumes,
    unit: Option<&'a Unit>,
    install: Option<&'a quadlet::Install>,
) -> impl Iterator<Item = color_eyre::Result<quadlet::File>> + 'a {
    volumes.into_iter().filter_map(move |(name, volume)| {
        volume.and_then(|volume| match volume {
            Resource::Compose(volume) => (!volume.is_empty()).then(|| {
                quadlet::Volume::try_from(volume)
                    .wrap_err_with(|| {
                        format!("error converting volume `{name}` into a Quadlet volume")
                    })
                    .map(|volume| quadlet::File {
                        name: name.into(),
                        unit: unit.cloned(),
                        resource: volume.into(),
                        globals: Globals::default(),
                        service: None,
                        install: install.cloned(),
                    })
            }),
            Resource::External { .. } => {
                Some(Err(eyre!("external volumes (`{name}`) are not supported")))
            }
        })
    })
}
