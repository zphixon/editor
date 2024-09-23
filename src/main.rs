use async_process::Command;
use regex::Regex;
use serde::{de::Visitor, Deserialize, Deserializer};
use std::{collections::HashMap, fmt::Display, net::SocketAddr, path::PathBuf};
use url::Url;
use warp::{
    filters::path::FullPath,
    http::{Response, StatusCode},
    reject::Rejection,
    Filter,
};

#[derive(Deserialize)]
struct Config {
    url: Url,
    blog_url: Url,
    blog_dir: PathBuf,
    dest_dir: PathBuf,
    build_command: String,
    #[serde(deserialize_with = "parse_regex")]
    path_regex: Regex,
    bind: SocketAddr,
    edit_template: PathBuf,
    create_revision: Vec<String>,
    revert_template: PathBuf,
    list_revisions: Vec<String>,
    revert_revision: Vec<String>,
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
        return Err(Err(warp::reject()));
    }

    Ok(actual_path)
}

async fn command_stdout(
    config: &Config,
    mut args: impl Iterator<Item = &str>,
) -> Result<String, Response<String>> {
    let mut command = Command::new(args.next().unwrap());
    for arg in args {
        command.arg(arg);
    }

    command.current_dir(&config.blog_dir);
    let output = command.output().await.map_err(|err| five_hundred(err))?;

    if !output.status.success() {
        return Err(five_hundred(String::from_utf8_lossy(&output.stderr)));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into())
}

#[tokio::main]
async fn main() {
    let config_buf = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let config: &'static Config = Box::leak(Box::new(toml::from_str(&config_buf).unwrap()));
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

                match tokio::fs::write(&actual_path, &content).await {
                    Ok(_) => {}
                    Err(_) => return Ok(five_hundred("couldn't write")),
                }

                let message = format!(
                    "edit {}",
                    actual_path
                        .strip_prefix(&config.blog_dir)
                        .unwrap()
                        .display()
                );

                let stdout = match command_stdout(
                    config,
                    config
                        .create_revision
                        .iter()
                        .map(|s| s.as_str())
                        .chain([message.as_str()]),
                )
                .await
                {
                    Ok(stdout) => stdout,
                    Err(fh) => return Ok(fh),
                };

                Ok::<_, Rejection>(
                    Response::builder()
                        .body(format!("wrote to {}\n{}", actual_path.display(), stdout))
                        .unwrap(),
                )
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
                        .replace("{{ action }}", config.url.as_str())
                        .replace("{{ content }}", &page_content)
                        .replace("{{ path }}", path_str),
                )
                .unwrap();

            Ok::<_, Rejection>(response)
        });

    let publish_get = warp::path("publish")
        .and(warp::path::full())
        .map(|path: FullPath| format!("publish {}", path.as_str()));

    let route = edit_get
        .or(edit_post)
        .or(publish_get)
        // .or(publish_post)
        .or(revert_get)
        .or(revert_post)
        .or(warp::any().and(warp::path::full()).map(|path: FullPath| {
            warp::reply::with_status(format!("404: {}", path.as_str()), StatusCode::NOT_FOUND)
        }));

    let server = warp::serve(route);
    server.bind(config.bind).await;
}
