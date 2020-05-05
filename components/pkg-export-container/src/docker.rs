use crate::{build::BuildRoot,
            error::{Error,
                    Result},
            naming::Naming,
            util,
            Credentials};
use failure::SyncFailure;
use habitat_common::ui::{Status,
                         UIWriter,
                         UI};
use habitat_core::package::PackageIdent;
use handlebars::Handlebars;
use serde_json;
use std::{fs,
          path::{Path,
                 PathBuf},
          str::FromStr};

// This code makes heavy use of `#[cfg(unix)]` and `#[cfg(windows)]`. This should potentially be
// changed to use the various target feature flags.

/// The `Dockerfile` template.
#[cfg(unix)]
const DOCKERFILE: &str = include_str!("../defaults/Dockerfile.hbs");
#[cfg(windows)]
const DOCKERFILE: &str = include_str!("../defaults/Dockerfile_win.hbs");
/// The build report template.
const BUILD_REPORT: &str = include_str!("../defaults/last_docker_export.env.hbs");

// TODO (CM): public temporarily
pub(crate) trait Identified {
    /// The base name of an image.
    fn name(&self) -> String;

    /// The possibly-empty list of tags for an image.
    fn tags(&self) -> Vec<String>;

    /// Returns a non-empty collection of names this image is known
    /// by.
    ///
    /// If an image has no tags, it includes just the name. If it
    /// *does* have tags, it includes the tags prepended with the
    /// name.
    ///
    /// Thus, you could get as little as:
    ///
    /// core/redis
    ///
    /// or as much as:
    ///
    /// core/redis:latest
    /// core/redis:4.0.14
    /// core/redis:4.0.14-20190319155852
    /// core/redis:latest
    /// core/redis:my-custom-tag
    fn expanded_identifiers(&self) -> Vec<String> {
        let mut ids = vec![];

        let tags = self.tags();
        let name = self.name();

        if tags.is_empty() {
            ids.push(name);
        } else {
            for tag in tags {
                ids.push(format!("{}:{}", name, tag));
            }
        }

        ids
    }
}

/// A builder used to create a Docker image.
pub struct ImageBuilder {
    /// The base workdir which hosts the root file system.
    workdir: PathBuf,
    /// The name for the image.
    name:    String,
    /// A list of tags for the image.
    tags:    Vec<String>,
    /// Optional memory limit to pass to pass to the docker build
    memory:  Option<String>,
}

impl Identified for ImageBuilder {
    fn name(&self) -> String { self.name.clone() }

    fn tags(&self) -> Vec<String> { self.tags.clone() }
}

impl ImageBuilder {
    fn new(workdir: &Path, name: &str) -> Self {
        ImageBuilder { workdir: workdir.to_path_buf(),
                       name:    name.to_string(),
                       tags:    Vec::new(),
                       memory:  None, }
    }

    /// Adds a tag for the Docker image.
    pub fn tag(mut self, tag: String) -> Self {
        self.tags.push(tag);
        self
    }

    /// Specifies an amount of memory to allocate to build
    pub fn memory(mut self, memory: &str) -> Self {
        self.memory = Some(memory.to_string());
        self
    }

    /// Builds the Docker image locally and returns the corresponding `DockerImage`.
    ///
    /// # Errors
    ///
    /// * If building the Docker image fails
    pub fn build(self) -> Result<DockerImage> {
        let mut cmd = util::docker_cmd();
        cmd.current_dir(&self.workdir)
           .arg("build")
           .arg("--force-rm");
        if let Some(ref mem) = self.memory {
            cmd.arg("--memory").arg(mem);
        }
        for identifier in &self.expanded_identifiers() {
            cmd.arg("--tag").arg(identifier);
        }
        cmd.arg(".");
        debug!("Running: {:?}", &cmd);
        let exit_status = cmd.spawn()?.wait()?;
        if !exit_status.success() {
            return Err(Error::BuildFailed(exit_status).into());
        }

        let id = match self.tags.first() {
            Some(tag) => self.image_id(&format!("{}:{}", &self.name, tag))?,
            None => self.image_id(&self.name)?,
        };

        Ok(DockerImage { id,
                         name: self.name,
                         tags: self.tags,
                         workdir: self.workdir.to_owned() })
    }

    fn image_id(&self, image_tag: &str) -> Result<String> {
        let mut cmd = util::docker_cmd();
        cmd.arg("images").arg("-q").arg(image_tag);
        debug!("Running: {:?}", &cmd);
        let output = cmd.output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        match stdout.lines().next() {
            Some(id) => Ok(id.to_string()),
            None => Err(Error::DockerImageIdNotFound(image_tag.to_string()).into()),
        }
    }
}

/// A built Docker image which exists locally.
pub struct DockerImage {
    /// The image ID for this image.
    id:      String,
    /// The name of this image.
    name:    String,
    /// The list of tags for this image.
    tags:    Vec<String>,
    /// The base workdir which hosts the root file system.
    workdir: PathBuf,
}

impl Identified for DockerImage {
    fn name(&self) -> String { self.name.clone() }

    fn tags(&self) -> Vec<String> { self.tags.clone() }
}

impl DockerImage {
    /// Pushes the Docker image, with all tags, to a remote registry using the provided
    /// `Credentials`.
    ///
    /// # Errors
    ///
    /// * If a registry login is not successful
    /// * If a pushing one or more of the image tags fails
    /// * If a registry logout is not successful
    pub fn push(&self,
                ui: &mut UI,
                credentials: &Credentials,
                registry_url: Option<&str>)
                -> Result<()> {
        ui.begin(format!("Pushing Docker image '{}' with all tags to remote registry",
                         self.name))?;
        self.create_docker_config_file(credentials, registry_url)
            .unwrap();

        for image_tag in self.expanded_identifiers() {
            ui.status(Status::Uploading,
                      format!("image '{}' to remote registry", image_tag))?;
            let mut cmd = util::docker_cmd();
            cmd.arg("--config");
            cmd.arg(self.workdir.to_str().unwrap());
            cmd.arg("push").arg(&image_tag);
            debug!("Running: {:?}", &cmd);
            let exit_status = cmd.spawn()?.wait()?;
            if !exit_status.success() {
                return Err(Error::PushImageFailed(exit_status).into());
            }
            ui.status(Status::Uploaded, format!("image '{}'", &image_tag))?;
        }

        ui.end(format!("Docker image '{}' published with tags: {}",
                       self.name,
                       self.tags.join(", "),))?;

        Ok(())
    }

    /// Removes the image from the local Docker engine along with all tags.
    ///
    /// # Errors
    ///
    /// * If one or more of the image tags cannot be removed
    pub fn rm(self, ui: &mut UI) -> Result<()> {
        ui.begin(format!("Cleaning up local Docker image '{}' with all tags",
                         self.name))?;

        for image_tag in self.expanded_identifiers() {
            ui.status(Status::Deleting, format!("local image '{}'", image_tag))?;
            let mut cmd = util::docker_cmd();
            cmd.arg("rmi").arg(image_tag);
            debug!("Running: {:?}", &cmd);
            let exit_status = cmd.spawn()?.wait()?;
            if !exit_status.success() {
                return Err(Error::RemoveImageFailed(exit_status).into());
            }
        }

        ui.end(format!("Local Docker image '{}' with tags: {} cleaned up",
                       self.name,
                       self.tags.join(", "),))?;
        Ok(())
    }

    /// Create a build report with image metadata in the given path.
    ///
    /// # Errors
    ///
    /// * If the destination directory cannot be created
    /// * If the report file cannot be written
    pub fn create_report<P: AsRef<Path>>(&self, ui: &mut UI, dst: P) -> Result<()> {
        let report = dst.as_ref().join("last_docker_export.env");
        ui.status(Status::Creating,
                  format!("build report {}", report.display()))?;
        fs::create_dir_all(&dst)?;
        let name_tags: Vec<_> = self.tags
                                    .iter()
                                    .map(|t| format!("{}:{}", &self.name, t))
                                    .collect();
        let json = json!({
            "id": &self.id,
            "name": &self.name,
            "tags": self.tags.join(","),
            "name_tags": name_tags.join(","),
        });
        util::write_file(&report,
                         &Handlebars::new().template_render(BUILD_REPORT, &json)
                                           .map_err(SyncFailure::new)?)?;
        Ok(())
    }

    pub fn create_docker_config_file(&self,
                                     credentials: &Credentials,
                                     registry_url: Option<&str>)
                                     -> Result<()> {
        let config = self.workdir.join("config.json");
        fs::create_dir_all(&self.workdir)?;
        let registry = match registry_url {
            Some(url) => url,
            None => "https://index.docker.io/v1/",
        };
        debug!("Using registry: {:?}", registry);
        let json = json!({
            "auths": {
                registry: {
                    "auth": credentials.token
                }
            }
        });
        util::write_file(&config, &serde_json::to_string(&json).unwrap())?;
        Ok(())
    }
}

/// A temporary file system build root for building a Docker image, based on Habitat packages.
pub struct DockerBuildRoot(BuildRoot);

impl DockerBuildRoot {
    /// Builds a completed Docker build root from a `BuildRoot`, performing any final tasks on the
    /// root file system.
    ///
    /// # Errors
    ///
    /// * If any remaining tasks cannot be performed in the build root
    #[cfg(unix)]
    pub fn from_build_root(build_root: BuildRoot, ui: &mut UI) -> Result<Self> {
        let root = DockerBuildRoot(build_root);
        root.add_users_and_groups(ui)?;
        root.create_entrypoint(ui)?;
        root.create_dockerfile(ui)?;

        Ok(root)
    }

    #[cfg(windows)]
    pub fn from_build_root(build_root: BuildRoot, ui: &mut UI) -> Result<Self> {
        let root = DockerBuildRoot(build_root);
        root.create_dockerfile(ui)?;

        Ok(root)
    }

    /// Destroys the temporary build root.
    ///
    /// Note that the build root will automatically destroy itself when it falls out of scope, so
    /// a call to this method is not required, but calling this will provide more user-facing
    /// progress and error reporting.
    ///
    /// # Errors
    ///
    /// * If the temporary work directory cannot be removed
    pub fn destroy(self, ui: &mut UI) -> Result<()> { self.0.destroy(ui) }

    #[cfg(unix)]
    fn add_users_and_groups(&self, ui: &mut UI) -> Result<()> {
        use std::{fs::OpenOptions,
                  io::Write};

        let ctx = self.0.ctx();
        let (users, groups) = ctx.svc_users_and_groups()?;
        {
            let file = "etc/passwd";
            let mut f = OpenOptions::new().append(true)
                                          .open(ctx.rootfs().join(&file))?;
            for user in users {
                ui.status(Status::Creating,
                          format!("user '{}' in /{}", user.name, &file))?;
                writeln!(f, "{}", user)?;
            }
        }
        {
            let file = "etc/group";
            let mut f = OpenOptions::new().append(true)
                                          .open(ctx.rootfs().join(&file))?;
            for group in groups {
                ui.status(Status::Creating,
                          format!("group '{}' in /{}", group.name, &file))?;
                writeln!(f, "{}", group)?;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn create_entrypoint(&self, ui: &mut UI) -> Result<()> {
        use habitat_core::util::posix_perm;

        /// The entrypoint script template.
        const INIT_SH: &str = include_str!("../defaults/init.sh.hbs");

        ui.status(Status::Creating, "entrypoint script")?;
        let ctx = self.0.ctx();
        let busybox_shell =
            util::pkg_path_for(&util::busybox_ident()?, ctx.rootfs())?.join("bin/sh");
        let json = json!({
            "busybox_shell": busybox_shell,
            "path": ctx.env_path(),
            "sup_bin": format!("{} sup", ctx.bin_path().join("hab").display()),
            "primary_svc_ident": ctx.primary_svc_ident().to_string(),
        });
        let init = ctx.rootfs().join("init.sh");
        util::write_file(&init,
                         &Handlebars::new().template_render(INIT_SH, &json)
                                           .map_err(SyncFailure::new)?)?;
        posix_perm::set_permissions(init.to_string_lossy().as_ref(), 0o0755)?;
        Ok(())
    }

    fn create_dockerfile(&self, ui: &mut UI) -> Result<()> {
        ui.status(Status::Creating, "image Dockerfile")?;
        let ctx = self.0.ctx();
        let json = json!({
            "base_image": ctx.base_image(),
            "rootfs": ctx.rootfs().file_name().expect("file_name exists")
                .to_string_lossy()
                .as_ref(),
            "path": ctx.env_path(),
            "hab_path": util::pkg_path_for(
                &PackageIdent::from_str("core/hab")?,
                ctx.rootfs())?.join("bin/hab")
                .to_string_lossy()
                .replace("\\", "/"),
            "exposes": ctx.svc_exposes().join(" "),
            "multi_layer": ctx.multi_layer(),
            "primary_svc_ident": ctx.primary_svc_ident().to_string(),
            "installed_primary_svc_ident": ctx.installed_primary_svc_ident()?.to_string(),
            "environment": ctx.environment,
            "packages": self.0.graph().reverse_topological_sort().iter().map(ToString::to_string).collect::<Vec<_>>(),
        });
        util::write_file(self.0.workdir().join("Dockerfile"),
                         &Handlebars::new().template_render(DOCKERFILE, &json)
                                           .map_err(SyncFailure::new)?)?;
        Ok(())
    }

    /// Build the Docker image locally using the provided naming policy.
    pub fn export(&self,
                  ui: &mut UI,
                  naming: &Naming,
                  memory: Option<&str>)
                  -> Result<DockerImage> {
        ui.status(Status::Creating, "Docker image")?;
        let ident = self.0.ctx().installed_primary_svc_ident()?;
        let channel = self.0.ctx().channel();

        // TODO (CM): Ideally, we'd toss this error much earlier,
        // since this error would be based on user input errors
        let (image_name, tags) = naming.image_identifiers(&ident, &channel)?;

        let mut builder = ImageBuilder::new(self.0.workdir(), &image_name);
        for tag in tags {
            builder = builder.tag(tag);
        }
        if let Some(memory) = memory {
            builder = builder.memory(memory);
        }
        builder.build()
    }
}
