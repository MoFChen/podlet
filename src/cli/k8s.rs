//! Kubernetes YAML [`File`] for converting a [`Compose`] file into a [`Pod`] and
//! [`PersistentVolumeClaim`]s.

mod service;
mod volume;

use std::fmt::{self, Display, Formatter};

use color_eyre::eyre::{ensure, OptionExt, WrapErr};
use compose_spec::{Compose, Resource};
use k8s_openapi::{
    api::core::v1::{PersistentVolumeClaim, Pod, PodSpec},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};

use self::service::Service;

/// A Kubernetes YAML file representing a [`Pod`] and optional [`PersistentVolumeClaim`]s.
///
/// Created by converting from a [`Compose`] file.
#[derive(Debug)]
pub struct File {
    /// The name of the file, without the extension.
    pub name: String,

    /// The Kubernetes [`Pod`].
    pub pod: Pod,

    /// Optional Kubernetes [`PersistentVolumeClaim`]s.
    ///
    /// Needed if a [`compose_spec::Volume`] has additional options set.
    pub persistent_volume_claims: Vec<PersistentVolumeClaim>,
}

impl TryFrom<Compose> for File {
    type Error = color_eyre::Report;

    fn try_from(
        Compose {
            version: _,
            name,
            include,
            services,
            networks,
            volumes,
            configs,
            secrets,
            extensions,
        }: Compose,
    ) -> Result<Self, Self::Error> {
        ensure!(include.is_empty(), "`include` is not supported");
        ensure!(networks.is_empty(), "`networks` is not supported");
        ensure!(configs.is_empty(), "`configs` is not supported");
        ensure!(secrets.is_empty(), "`secrets` is not supported");
        ensure!(
            extensions.is_empty(),
            "compose extensions are not supported"
        );

        let name = name.map(String::from).ok_or_eyre("`name` is required")?;

        let spec =
            services
                .into_iter()
                .try_fold(PodSpec::default(), |mut spec, (name, service)| {
                    Service::from_compose(&name, service)
                        .add_to_pod_spec(&mut spec)
                        .wrap_err_with(|| {
                            format!("error adding service `{name}` to Kubernetes pod spec")
                        })
                        .map(|()| spec)
                })?;

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(spec),
            status: None,
        };

        let persistent_volume_claims = volumes
            .into_iter()
            .filter_map(|(name, volume)| match volume {
                Some(Resource::Compose(volume)) if !volume.is_empty() => Some(
                    volume::try_into_persistent_volume_claim(name.clone(), volume).wrap_err_with(
                        || format!("error converting volume `{name}` to a persistent volume claim"),
                    ),
                ),
                _ => None,
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            name,
            pod,
            persistent_volume_claims,
        })
    }
}

impl Display for File {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let Self {
            name: _,
            pod,
            persistent_volume_claims,
        } = self;

        for volume in persistent_volume_claims {
            f.write_str(&serde_yaml::to_string(volume).map_err(|_| fmt::Error)?)?;
            writeln!(f, "---")?;
        }

        f.write_str(&serde_yaml::to_string(pod).map_err(|_| fmt::Error)?)
    }
}
