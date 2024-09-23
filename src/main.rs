use async_process::Command;
use regex::Regex;
use serde::{de::Visitor, Deserialize, Deserializer};
use std::{
    collections::HashMap,
    fmt::Display,
    future::Future,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};
use url::Url;
use warp::{
    filters::path::FullPath,
    http::{Response, StatusCode},
    reject::Rejection,
    Filter,
};

#[derive(Deserialize)]
struct Config {
    bind: SocketAddr,
    url: Url,

    blog_url: Url,
    #[serde(deserialize_with = "parse_regex")]
    path_regex: Regex,
    blog_dir: PathBuf,
    blog_build_dir: PathBuf,
    dest_dir: PathBuf,

    build_command: Vec<String>,
    create_revision: Vec<String>,
    stage_revision: Vec<String>,
    reset_command: Vec<String>,
    list_revisions: Vec<String>,
    revert_revision: Vec<String>,

    draft_template: PathBuf,
    publish_template: PathBuf,
    edit_template: PathBuf,
    revert_template: PathBuf,
}

fn parse_regex<'de, D>(de: D) -> Result<Regex, D::Error>
where
    D: Deserializer<'de>,
{
    struct RegexVisitor {}
    impl Visitor<'_> for RegexVisitor {
        type Value = Regex;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(formatter, "URL")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Regex::new(v).map_err(|err| E::custom(format!("{err}")))
        }
    }

    de.deserialize_str(RegexVisitor {})
}

fn five_hundred<F: Display>(body: F) -> Response<String> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(format!("{}", body))
        .unwrap()
}

async fn path_to_file(
    config: &Config,
    path: &str,
) -> Result<PathBuf, Result<Response<String>, Rejection>> {
    //let blog_url = format!("{}{}", config.blog_url, path).replace("//", "/");
    let blog_url = config.blog_url.join(path).unwrap();

    let blog_response = match reqwest::get(blog_url).await {
        Ok(response) => response,
        Err(err) => {
            return Err(Ok(five_hundred(err)));
        }
    };

    if !blog_response.status().is_success() {
        return Err(Ok(Response::builder()
            .header("Content-Type", "text/html")
            .body(format!(
                "<head><meta http-equiv=\"Refresh\" content=\"0; URL={}publish{}\"></head>",
                config.url, path
            ))
            .unwrap()));
    }

    let blog_text = match blog_response.text().await {
        Ok(text) => text,
        Err(err) => return Err(Ok(five_hundred(err))),
    };

    let relative_path = match config.path_regex.captures(&blog_text) {
        Some(captures) => captures,
        None => {
            return Err(Ok(five_hundred(format!(
                "nothing matching {} in {}",
                config.path_regex, blog_text
            ))))
        }
    };

    let mut page_path = config.blog_dir.clone();
    page_path.push(&relative_path[1]);
    let actual_path = page_path
        .canonicalize()
        .map_err(|_| warp::reject())
        .map_err(Err)?;

    if !actual_path.starts_with(&config.blog_dir) {
        println!(
            "cheating bastard: {} does NOT start with {}",
            actual_path.display(),
            config.blog_dir.display()
        );
        return Err(Err(warp::reject()));
    }

    Ok(actual_path)
}

async fn command_stdout(
    config: &Config,
    args: impl Iterator<Item = &str>,
) -> Result<String, Response<String>> {
    let args = args.collect::<Vec<&str>>();
    let mut command = Command::new(args[0]);
    for arg in &args[1..] {
        command.arg(arg);
    }

    command.current_dir(&config.blog_dir);
    let output = command.output().await.map_err(|err| five_hundred(err))?;

    if !output.status.success() {
        let all_output = String::from("failed: ")
            + &args.join(" ")
            + "\nstdout:\n"
            + &String::from_utf8_lossy(&output.stdout)
            + "\nstderr:\n"
            + &String::from_utf8_lossy(&output.stderr);
        return Err(five_hundred(all_output));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into())
}

// https://stackoverflow.com/a/65192210
async fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> tokio::io::Result<()> {
    tokio::fs::create_dir_all(&dst).await?;
    let mut readdir = tokio::fs::read_dir(src).await?;
    while let Some(entry) = readdir.next_entry().await? {
        let ty = entry.file_type().await?;
        if ty.is_dir() {
            Box::pin(copy_dir_all(
                entry.path(),
                dst.as_ref().join(entry.file_name()),
            ))
            .await?;
        } else {
            tokio::fs::copy(entry.path(), dst.as_ref().join(entry.file_name())).await?;
        }
    }
    Ok(())
}

async fn rebuild(config: &Config) -> Result<String, Response<String>> {
    let stdout = command_stdout(config, config.build_command.iter().map(|s| s.as_str())).await?;

    if tokio::fs::try_exists(&config.dest_dir)
        .await
        .map_err(five_hundred)?
    {
        tokio::fs::remove_dir_all(&config.dest_dir)
            .await
            .map_err(five_hundred)?;
    }

    copy_dir_all(&config.blog_build_dir, &config.dest_dir)
        .await
        .map_err(five_hundred)?;

    Ok(stdout)
}

async fn reset_if_err(
    config: &Config,
    f: impl Future<Output = Result<String, Response<String>>>,
) -> Result<String, Response<String>> {
    match f.await {
        Ok(ok) => Ok(ok),
        Err(mut err) => {
            match command_stdout(config, config.reset_command.iter().map(|s| s.as_str())).await {
                Ok(ok) => err
                    .body_mut()
                    .push_str(&format!("\n\nhad to reset\n\n{}", ok)),
                Err(err2) => err
                    .body_mut()
                    .push_str(&format!("\n\nfailed resetting\n\n{}", err2.body())),
            }
            Err(err)
        }
    }
}

async fn set_content_with_revision(
    config: &Config,
    actual_path: &Path,
    content: &str,
    note: Option<&str>,
) -> Result<String, Response<String>> {
    match tokio::fs::write(&actual_path, &content).await {
        Ok(_) => {}
        Err(_) => return Err(five_hundred("couldn't write")),
    }

    let message = format!(
        "{}edit {}",
        if let Some(note) = note {
            format!("{} - ", note)
        } else {
            String::new()
        },
        actual_path
            .strip_prefix(&config.blog_dir)
            .unwrap()
            .display()
    );

    create_revision(config, actual_path, message).await
}

async fn create_revision(
    config: &Config,
    actual_path: &Path,
    message: String,
) -> Result<String, Response<String>> {
    let path = format!("{}", actual_path.display());

    let mut stdout = rebuild(config).await?;

    stdout.push_str(
        &command_stdout(
            config,
            config
                .stage_revision
                .iter()
                .map(|s| s.as_str())
                .chain([path.as_str()]),
        )
        .await?,
    );

    stdout.push_str(
        &command_stdout(
            config,
            config
                .create_revision
                .iter()
                .map(|s| s.as_str())
                .chain([message.as_str()]),
        )
        .await?,
    );

    stdout.push_str(&rebuild(config).await?);

    Ok(stdout)
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

#[tokio::main]
async fn main() {
    let config_buf = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();

    let mut config: Config = toml::from_str(&config_buf).unwrap();
    config.blog_dir = config.blog_dir.canonicalize().unwrap();
    config.blog_build_dir = config.blog_build_dir.canonicalize().unwrap();
    config.dest_dir = config.dest_dir.canonicalize().unwrap();
    config.draft_template = config.draft_template.canonicalize().unwrap();
    config.publish_template = config.publish_template.canonicalize().unwrap();
    config.edit_template = config.edit_template.canonicalize().unwrap();
    config.revert_template = config.revert_template.canonicalize().unwrap();

    let config: &'static Config = Box::leak(Box::new(config));

    let edit_template: &'static str = Box::leak(
        std::fs::read_to_string(&config.edit_template)
            .unwrap()
            .into_boxed_str(),
    );

    let revert_template: &'static str = Box::leak(
        std::fs::read_to_string(&config.revert_template)
            .unwrap()
            .into_boxed_str(),
    );

    let publish_template: &'static str = Box::leak(
        std::fs::read_to_string(&config.publish_template)
            .unwrap()
            .into_boxed_str(),
    );

    let draft_template: &'static str = Box::leak(
        std::fs::read_to_string(&config.draft_template)
            .unwrap()
            .into_boxed_str(),
    );

    let revert_get =
        warp::get()
            .and(warp::path("revert"))
            .and_then(move || async move {
                let stdout =
                    match command_stdout(config, config.list_revisions.iter().map(|s| s.as_str()))
                        .await
                    {
                        Ok(stdout) => stdout,
                        Err(fh) => return Ok(fh),
                    };

                Ok::<_, Rejection>(
                    Response::builder()
                        .body(
                            revert_template
                                .replace("{{ action }}", config.url.as_str())
                                .replace(
                                    "{{ revisions }}",
                                    &stdout.replace("\n", "\\n").replace("\"", "\\\""),
                                ),
                        )
                        .unwrap(),
                )
            });

    let revert_post = warp::post()
        .and(warp::path("revert"))
        .and(warp::filters::body::form())
        .and_then(move |form: HashMap<String, String>| async move {
            let Some(revision_name) = form.get("revision") else {
                return Ok(five_hundred("no revision from form?"));
            };

            let Some(revision) = revision_name.split_whitespace().next() else {
                return Ok(five_hundred(format!(
                    "no hash in revision {}",
                    revision_name
                )));
            };

            let stdout = match command_stdout(
                config,
                config
                    .revert_revision
                    .iter()
                    .map(|s| s.as_str())
                    .chain([revision]),
            )
            .await
            {
                Ok(out) => out,
                Err(fh) => return Ok(fh),
            };

            Ok::<_, Rejection>(Response::builder().body(stdout).unwrap())
        });

    let edit_post = warp::post()
        .and(warp::path("edit"))
        .and(warp::path::full())
        .and(warp::filters::body::form())
        .and_then(
            move |path: FullPath, form: HashMap<String, String>| async move {
                let path_str = path.as_str().strip_prefix("/edit").unwrap();
                let actual_path = match path_to_file(config, path_str).await {
                    Ok(path) => path,
                    Err(err) => return err,
                };

                let Some(content) = form.get("content") else {
                    return Ok(five_hundred("no content from form?"));
                };

                if form.get("delete").map(|s| s.as_str()) == Some("on") {
                    match tokio::fs::remove_file(&actual_path).await {
                        Ok(_) => {}
                        Err(err) => return Ok(five_hundred(err)),
                    };
                    let stdout = match reset_if_err(
                        config,
                        create_revision(
                            config,
                            &actual_path,
                            format!("delete {}", actual_path.display()),
                        ),
                    )
                    .await
                    {
                        Ok(stdout) => stdout,
                        Err(err) => return Ok(err),
                    };
                    Ok::<_, Rejection>(
                        Response::builder()
                            .body(format!("deleted {}\n\n{}", actual_path.display(), stdout))
                            .unwrap(),
                    )
                } else {
                    let stdout = match reset_if_err(
                        config,
                        set_content_with_revision(
                            config,
                            actual_path.as_path(),
                            content.as_str(),
                            form.get("note").map(|s| s.as_str()),
                        ),
                    )
                    .await
                    {
                        Ok(stdout) => stdout,
                        Err(err) => return Ok(err),
                    };
                    Ok::<_, Rejection>(
                        Response::builder()
                            .body(format!("wrote to {}\n\n{}", actual_path.display(), stdout))
                            .unwrap(),
                    )
                }
            },
        );

    let edit_get = warp::get()
        .and(warp::path("edit"))
        .and(warp::path::full())
        .and_then(move |path: FullPath| async move {
            let path_str = path.as_str().strip_prefix("/edit").unwrap();
            let actual_path = match path_to_file(config, path_str).await {
                Ok(path) => path,
                Err(err) => return err,
            };

            let page_content = match tokio::fs::read_to_string(&actual_path).await {
                Ok(content) => content,
                Err(_) => {
                    return Ok(five_hundred(format!(
                        "couldn't read {}",
                        actual_path.display()
                    )))
                }
            };

            let response = Response::builder()
                .header("Content-Type", "text/html")
                .body(
                    edit_template
                        .replace("{{ draftwidget }}", draft_template)
                        .replace("{{ action }}", config.url.as_str())
                        .replace("{{ request }}", "edit")
                        .replace("{{ content }}", &page_content)
                        .replace("{{ path }}", path_str)
                        .replace("{{ cookiename }}", "editdraft"),
                )
                .unwrap();

            Ok::<_, Rejection>(response)
        });

    let publish_get = warp::get()
        .and(warp::path("publish"))
        .and_then(move || async move {
            let response = Response::builder()
                .header("Content-Type", "text/html")
                .body(
                    publish_template
                        .replace("{{ draftwidget }}", draft_template)
                        .replace("{{ request }}", "publish")
                        .replace("{{ action }}", config.url.as_str())
                        .replace("{{ cookiename }}", "publishdraft"),
                )
                .unwrap();
            Ok::<_, Rejection>(response)
        });

    let publish_post = warp::post()
        .and(warp::path("publish"))
        .and(warp::filters::body::form())
        .and_then(move |form: HashMap<String, String>| async move {
            println!("{:?}", form);

            let Some(filename) = form.get("filename") else {
                // 400
                return Ok(five_hundred("missing filename"));
            };

            let Some(content) = form.get("content") else {
                return Ok(five_hundred("missing content"));
            };

            let actual_path = normalize_path(config.blog_dir.join(filename).as_path());
            if !actual_path.starts_with(&config.blog_dir) {
                println!("cheating bastard: {}", actual_path.display());
                return Err(warp::reject());
            }

            let stdout = match reset_if_err(
                config,
                set_content_with_revision(
                    config,
                    actual_path.as_path(),
                    content.as_str(),
                    form.get("note").map(|s| s.as_str()),
                ),
            )
            .await
            {
                Ok(stdout) => stdout,
                Err(err) => return Ok(err),
            };

            Ok::<_, Rejection>(
                Response::builder()
                    .body(format!("wrote to {}\n\n{}", actual_path.display(), stdout))
                    .unwrap(),
            )
        });

    let route = edit_get
        .or(edit_post)
        .or(publish_get)
        .or(publish_post)
        .or(revert_get)
        .or(revert_post)
        .or(warp::any().and(warp::path::full()).map(|path: FullPath| {
            warp::reply::with_status(format!("404: {}", path.as_str()), StatusCode::NOT_FOUND)
        }));

    let server = warp::serve(route);
    server.bind(config.bind).await;
}
