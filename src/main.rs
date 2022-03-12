// jkcoxson

use backend::Backend;
use bytes::BufMut;
use futures::TryStreamExt;
use rusty_libimobiledevice::plist::{Plist, PlistDictIter};
use rusty_libimobiledevice::{instproxy::InstProxyClient, libimobiledevice};
use std::{
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use warp::{
    filters::BoxedFilter,
    http::Uri,
    multipart::{FormData, Part},
    path::FullPath,
    redirect, Filter, Rejection, Reply,
};

mod backend;
mod config;
mod packets;

#[tokio::main]
async fn main() {
    let config = config::Config::load();
    let current_dir = std::env::current_dir().expect("failed to read current directory");
    let backend = Arc::new(Mutex::new(backend::Backend::load(&config)));
    let upload_backend = backend.clone();
    let status_backend = backend.clone();
    let list_apps_backend = backend.clone();
    let shortcuts_launch_backend = backend.clone();

    // Upload route
    let upload_route = warp::path("upload")
        .and(warp::post())
        .and(warp::multipart::form().max_length(5_000_000))
        .and(warp::filters::addr::remote())
        .and_then(move |form, addr| upload_file(form, addr, upload_backend.clone()));

    // Status route
    let status_route = warp::path("status")
        .and(warp::post())
        .and(warp::filters::addr::remote())
        .and_then(move |addr| status(addr, status_backend.clone()));

    // Admin route
    let admin_route = warp::path("admin").map(|| {
        warp::redirect(Uri::from_static(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ", // haha gottem
        ))
    });

    // Shortcuts route
    let list_apps_route = warp::path!("shortcuts" / "list_apps")
        .and(warp::get())
        .and(warp::filters::addr::remote())
        .and_then(move |addr| list_apps(addr, list_apps_backend.clone()));

    let shortcuts_launch_route = warp::path!("shortcuts" / "launch" / String)
        .and(warp::post())
        .and(warp::filters::addr::remote())
        .and_then(move |query, addr| shortcuts_run(query, addr, shortcuts_launch_backend.clone()));

    let routes = root_redirect()
        .or(warp::fs::dir(current_dir.join("../JitStreamerSite/dist")))
        .or(upload_route)
        .or(status_route)
        .or(list_apps_route)
        .or(shortcuts_launch_route)
        .or(admin_route);

    let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("Invalid address");
    warp::serve(routes).run(addr).await;
}

fn root_redirect() -> BoxedFilter<(impl Reply,)> {
    warp::path::full()
        .and_then(move |path: FullPath| async move {
            let path = path.as_str();

            // do not redirect if the path ends in a trailing slash
            // or contains a period (indicating a specific file, e.g. style.css)
            if path.ends_with("/") || path.contains(".") {
                return Err(warp::reject());
            }

            Ok(redirect::redirect(
                Uri::from_str(&[path, "/"].concat()).unwrap(),
            ))
        })
        .boxed()
}

async fn upload_file(
    form: FormData,
    address: Option<SocketAddr>,
    backend: Arc<Mutex<Backend>>,
) -> Result<impl Reply, Rejection> {
    let mut backend = backend.lock().await;
    let parts: Vec<Part> = form.try_collect().await.map_err(|e| {
        eprintln!("form error: {}", e);
        warp::reject::reject()
    })?;

    for p in parts {
        if p.name() == "file" {
            let value = p
                .stream()
                .try_fold(Vec::new(), |mut vec, data| {
                    vec.put(data);
                    async move { Ok(vec) }
                })
                .await
                .map_err(|e| {
                    eprintln!("reading file error: {}", e);
                    warp::reject::reject()
                })?;

            // Get string from value
            let value = match String::from_utf8(value) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(packets::upload_response(false, "Unable to read file"));
                }
            };
            // Attempt to parse it as an Apple Plist
            let plist: Plist = Plist::from_xml(value.clone()).unwrap();
            let udid = match plist.dict_get_item("UDID") {
                Ok(s) => s.get_string_val().unwrap(),
                _ => {
                    return Ok(packets::upload_response(false, "Invalid pairing file!"));
                }
            };
            let address = match address {
                Some(address) => address,
                None => {
                    return Ok(packets::upload_response(false, "No address provided"));
                }
            };
            let plist: Plist = Plist::from_xml(value).unwrap();
            // Save the plist to the plist storage directory
            match backend.write_pairing_file(plist.to_string(), &udid) {
                Ok(_) => {}
                Err(_) => {
                    return Ok(packets::upload_response(
                        false,
                        "Unable to save pairing file",
                    ));
                }
            }

            match backend.register_client(address.ip().to_string(), udid) {
                Ok(_) => {}
                Err(_) => {
                    return Ok(packets::upload_response(false, "Client already registered"));
                }
            }
            return Ok(packets::upload_response(true, ""));
        }
    }
    return Ok(packets::upload_response(false, "No file found"));
}

async fn status(
    addr: Option<SocketAddr>,
    backend: Arc<Mutex<Backend>>,
) -> Result<impl Reply, Rejection> {
    let mut backend = backend.lock().await;
    if let None = addr {
        return Ok(packets::status_packet(false, false));
    }
    if !addr.unwrap().to_string().starts_with(&backend.allowed_ip) {
        return Ok(packets::status_packet(false, false));
    }
    match backend.get_by_ip(&addr.unwrap().ip().to_string()) {
        Some(client) => {
            let start = SystemTime::now();
            let since_the_epoch = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            client.last_seen = since_the_epoch.as_secs();
        }
        None => return Ok(packets::status_packet(true, false)),
    };

    Ok(packets::status_packet(true, true))
}

async fn list_apps(
    addr: Option<SocketAddr>,
    backend: Arc<Mutex<Backend>>,
) -> Result<impl Reply, Rejection> {
    let mut backend = backend.lock().await;
    if let None = addr {
        println!("No address provided");
        return Err(warp::reject());
    }
    if !addr.unwrap().to_string().starts_with(&backend.allowed_ip) {
        println!("Address not allowed");
        return Err(warp::reject());
    }
    let client = match backend.get_by_ip(&addr.unwrap().ip().to_string()) {
        Some(client) => client,
        None => {
            println!("No client found with the given IP");
            return Err(warp::reject());
        }
    };
    let device = match libimobiledevice::get_device(client.udid.clone()) {
        Ok(device) => device,
        Err(_) => {
            println!("Unable to get device");
            return Err(warp::reject());
        }
    };

    let instproxy_client = match device.new_instproxy_client("jitstreamer".to_string()) {
        Ok(instproxy) => instproxy,
        Err(e) => {
            println!("Error starting instproxy: {:?}", e);
            return Err(warp::reject());
        }
    };
    let mut client_opts = InstProxyClient::options_new();
    InstProxyClient::options_add(
        &mut client_opts,
        vec![("ApplicationType".to_string(), Plist::new_string("Any"))],
    );
    InstProxyClient::options_set_return_attributes(
        &mut client_opts,
        vec![
            "CFBundleIdentifier".to_string(),
            "CFBundleExecutable".to_string(),
            "Container".to_string(),
        ],
    );
    let lookup_results = match instproxy_client.lookup(vec![], client_opts) {
        Ok(apps) => apps,
        Err(e) => {
            println!("Error looking up apps: {:?}", e);
            return Err(warp::reject());
        }
    };

    let mut p_iter = PlistDictIter::from(lookup_results);
    let mut apps = Vec::new();
    loop {
        let app = match p_iter.next_item() {
            Some(app) => app,
            None => break,
        };
        apps.push(app.0);
    }

    let mut to_ret = serde_json::Value::Object(serde_json::Map::new());
    for i in apps {
        if i.starts_with("com.apple.") || i.starts_with("com.google.") {
            continue;
        }
        to_ret[i] = serde_json::Value::String(i.to_string());
    }
    Ok(to_ret.to_string())
}

async fn shortcuts_run(
    app: String,
    addr: Option<SocketAddr>,
    backend: Arc<Mutex<Backend>>,
) -> Result<impl Reply, Rejection> {
    let mut backend = backend.lock().await;
    if let None = addr {
        println!("No address provided");
        return Err(warp::reject());
    }
    if !addr.unwrap().to_string().starts_with(&backend.allowed_ip) {
        println!("Address not allowed");
        return Err(warp::reject());
    }
    let client = match backend.get_by_ip(&addr.unwrap().ip().to_string()) {
        Some(client) => client,
        None => {
            println!("No client found with the given IP");
            return Err(warp::reject());
        }
    };
    let device = match libimobiledevice::get_device(client.udid.clone()) {
        Ok(device) => device,
        Err(_) => {
            println!("Unable to get device");
            return Err(warp::reject());
        }
    };

    let instproxy_client = match device.new_instproxy_client("idevicedebug".to_string()) {
        Ok(instproxy) => instproxy,
        Err(e) => {
            println!("Error starting instproxy: {:?}", e);
            return Err(warp::reject());
        }
    };

    let mut client_opts = InstProxyClient::options_new();
    InstProxyClient::options_add(
        &mut client_opts,
        vec![("ApplicationType".to_string(), Plist::new_string("Any"))],
    );
    InstProxyClient::options_set_return_attributes(
        &mut client_opts,
        vec![
            "CFBundleIdentifier".to_string(),
            "CFBundleExecutable".to_string(),
            "Container".to_string(),
        ],
    );
    let lookup_results = match instproxy_client.lookup(vec![app.clone()], client_opts) {
        Ok(apps) => apps,
        Err(e) => {
            println!("Error looking up apps: {:?}", e);
            return Err(warp::reject());
        }
    };
    let lookup_results = lookup_results.dict_get_item(&app).unwrap();

    let working_directory = match lookup_results.dict_get_item("Container") {
        Ok(p) => p,
        Err(_) => {
            println!("App not found");
            return Err(warp::reject());
        }
    };

    let working_directory = match working_directory.get_string_val() {
        Ok(p) => p,
        Err(_) => {
            println!("App not found");
            return Err(warp::reject());
        }
    };
    println!("Working directory: {}", working_directory);

    let bundle_path = match instproxy_client.get_path_for_bundle_identifier(app) {
        Ok(p) => p,
        Err(e) => {
            println!("Error getting path for bundle identifier: {:?}", e);
            return Err(warp::reject());
        }
    };

    println!("Bundle Path: {}", bundle_path);

    let debug_server = match device.new_debug_server("idevicedebug") {
        Ok(d) => d,
        Err(e) => {
            println!("Error starting debug server: {:?}", e);
            println!("Maybe mount the Developer DMG?");
            return Err(warp::reject());
        }
    };

    match debug_server.send_command("QSetMaxPacketSize: 1024".into()) {
        Ok(res) => println!("Successfully set max packet size: {:?}", res),
        Err(e) => {
            println!("Error setting max packet size: {:?}", e);
            return Err(warp::reject());
        }
    }

    match debug_server.send_command(format!("QSetWorkingDir: {}", working_directory).into()) {
        Ok(res) => println!("Successfully set working directory: {:?}", res),
        Err(e) => {
            println!("Error setting working directory: {:?}", e);
            return Err(warp::reject());
        }
    }

    match debug_server.set_argv(vec![bundle_path.clone(), bundle_path.clone()]) {
        Ok(res) => println!("Successfully set argv: {:?}", res),
        Err(e) => {
            println!("Error setting argv: {:?}", e);
            return Err(warp::reject());
        }
    }

    match debug_server.send_command("qLaunchSuccess".into()) {
        Ok(res) => println!("Got launch response: {:?}", res),
        Err(e) => {
            println!("Error checking if app launched: {:?}", e);
            return Err(warp::reject());
        }
    }

    match debug_server.send_command("D".into()) {
        Ok(res) => println!("Detaching: {:?}", res),
        Err(e) => {
            println!("Error detaching: {:?}", e);
            return Err(warp::reject());
        }
    }

    Ok("success")
}
