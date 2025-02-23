use std::io::{self, IsTerminal};

use anyhow::anyhow;
use clap::Parser;
use reqwest::Url;
use rover_client::operations::subgraph::routing_url::{self, SubgraphRoutingUrlInput};
use serde::Serialize;

use crate::options::{GraphRefOpt, ProfileOpt, SchemaOpt, SubgraphOpt};
use crate::utils::client::StudioClientConfig;
use crate::{RoverError, RoverErrorSuggestion, RoverOutput, RoverResult};

use rover_client::operations::subgraph::publish::{self, SubgraphPublishInput};
use rover_client::shared::GitContext;
use rover_std::Style;

#[derive(Debug, Serialize, Parser)]
pub struct Publish {
    #[clap(flatten)]
    graph: GraphRefOpt,

    #[clap(flatten)]
    subgraph: SubgraphOpt,

    #[clap(flatten)]
    profile: ProfileOpt,

    #[clap(flatten)]
    #[serde(skip_serializing)]
    schema: SchemaOpt,

    /// Indicate whether to convert a non-federated graph into a subgraph
    #[arg(short, long)]
    convert: bool,

    /// Url of a running subgraph that a supergraph can route operations to
    /// (often a deployed subgraph). May be left empty ("") or a placeholder url
    /// if not running a gateway or router in managed federation mode
    #[arg(long)]
    #[serde(skip_serializing)]
    routing_url: Option<String>,

    /// Bypasses warnings and the prompt to confirm publish when the routing url
    /// is invalid in TTY environment. In a future major version, this flag will
    /// be required to publish in a non-TTY environment. For now it will warn
    /// and publish anyway.
    #[arg(long)]
    allow_invalid_routing_url: bool,
}

impl Publish {
    pub fn run(
        &self,
        client_config: StudioClientConfig,
        git_context: GitContext,
    ) -> RoverResult<RoverOutput> {
        // if --allow-invalid-routing-url is not provided, we need to inspect
        // the URL and possibly prompt the user to publish
        if !self.allow_invalid_routing_url {
            Self::handle_maybe_invalid_routing_url(
                &self.routing_url,
                &mut io::stderr(),
                &mut io::stdin(),
                io::stderr().is_terminal() && io::stdin().is_terminal(),
            )?;
        }

        let client = client_config.get_authenticated_client(&self.profile)?;

        if self.routing_url.is_none() {
            let fetch_response = routing_url::run(
                SubgraphRoutingUrlInput {
                    graph_ref: self.graph.graph_ref.clone(),
                    subgraph_name: self.subgraph.subgraph_name.clone(),
                },
                &client,
            )?;

            Self::handle_maybe_invalid_routing_url(
                &Some(fetch_response),
                &mut io::stderr(),
                &mut io::stdin(),
                io::stderr().is_terminal() && io::stdin().is_terminal(),
            )?;
        }

        eprintln!(
            "Publishing SDL to {} (subgraph: {}) using credentials from the {} profile.",
            Style::Link.paint(self.graph.graph_ref.to_string()),
            Style::Link.paint(&self.subgraph.subgraph_name),
            Style::Command.paint(&self.profile.profile_name)
        );

        let schema = self
            .schema
            .read_file_descriptor("SDL", &mut std::io::stdin())?;

        tracing::debug!("Publishing \n{}", &schema);

        let publish_response = publish::run(
            SubgraphPublishInput {
                graph_ref: self.graph.graph_ref.clone(),
                subgraph: self.subgraph.subgraph_name.clone(),
                url: self.routing_url.clone(),
                schema,
                git_context,
                convert_to_federated_graph: self.convert,
            },
            &client,
        )?;

        Ok(RoverOutput::SubgraphPublishResponse {
            graph_ref: self.graph.graph_ref.clone(),
            subgraph: self.subgraph.subgraph_name.clone(),
            publish_response,
        })
    }

    fn handle_maybe_invalid_routing_url(
        maybe_invalid_routing_url: &Option<String>,
        // For testing purposes, we pass in stub `Write`er and `Read`ers to
        // simulate input and verify output.
        writer: &mut impl io::Write,
        reader: &mut impl io::Read,
        // Simulate a CI environment (non-TTY) for testing
        is_atty: bool,
    ) -> RoverResult<()> {
        // if a --routing-url is provided AND the URL is unparsable,
        // we need to warn and prompt the user, else we can assume a publish
        if let Some(routing_url) = maybe_invalid_routing_url {
            match Url::parse(routing_url) {
                Ok(parsed_url) => {
                    tracing::debug!("Parsed URL: {}", parsed_url.to_string());
                    let reason = format!("`{}` is not a valid routing URL. The `{}` protocol is not supported by the router. Valid protocols are `http` and `https`.", Style::Link.paint(routing_url), &parsed_url.scheme());
                    if !["http", "https"].contains(&parsed_url.scheme()) {
                        if is_atty {
                            Self::prompt_for_publish(
                                format!("{reason} Continuing the publish will make this subgraph unreachable by your supergraph. Would you still like to publish?").as_str(),
                                reader,
                                writer,
                            )?;
                        } else {
                            Self::non_tty_hard_error(&reason)?;
                        }
                    } else if let Some(host) = parsed_url.host_str() {
                        if ["localhost", "127.0.0.1"].contains(&host) {
                            let reason = format!("The host `{}` is not routable via the public internet. Continuing the publish will make this subgraph reachable in local environments only.", host);
                            if is_atty {
                                Self::prompt_for_publish(
                                    format!("{reason} Would you still like to publish?").as_str(),
                                    reader,
                                    writer,
                                )?;
                            } else {
                                Self::non_tty_warn_about_local_url(&reason, writer)?;
                            }
                        }
                    }
                }
                Err(parse_error) => {
                    tracing::debug!("Parse error: {}", parse_error.to_string());
                    let reason = format!(
                        "`{}` is not a valid routing URL.",
                        Style::Link.paint(routing_url)
                    );
                    if is_atty {
                        Self::prompt_for_publish(
                        format!("{} Continuing the publish will make this subgraph unreachable by your supergraph. Would you still like to publish?", &reason).as_str(),
                            reader,
                            writer,
                        )?;
                    } else {
                        Self::non_tty_hard_error(&reason)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn prompt_for_publish(
        message: &str,
        reader: &mut impl io::Read,
        writer: &mut impl io::Write,
    ) -> RoverResult<Option<bool>> {
        write!(writer, "{} [y/N] ", message)?;
        let mut response = [0];
        reader.read_exact(&mut response)?;
        if std::str::from_utf8(&response).unwrap().to_lowercase() == *"y" {
            Ok(Some(true))
        } else {
            Err(anyhow!("You cancelled a subgraph publish due to an invalid routing url.").into())
        }
    }

    pub fn non_tty_hard_error(reason: &str) -> RoverResult<()> {
        Err(RoverError::new(anyhow!("{reason}"))
            .with_suggestion(RoverErrorSuggestion::AllowInvalidRoutingUrlOrSpecifyValidUrl))
    }

    pub fn non_tty_warn_about_local_url(
        reason: &str,
        writer: &mut dyn io::Write,
    ) -> RoverResult<()> {
        writeln!(writer, "{} {reason}", Style::WarningPrefix.paint("WARN:"),)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::command::subgraph::Publish;

    #[test]
    fn test_confirm_invalid_url_publish() {
        let mut input = "y".as_bytes();
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("invalid-url".to_string()),
            &mut output,
            &mut input,
            true,
        );

        assert!(result.is_ok());
        assert!(input.is_empty());
        assert!(std::str::from_utf8(&output).unwrap().contains("is not a valid routing URL. Continuing the publish will make this subgraph unreachable by your supergraph. Would you still like to publish?"));
    }

    #[test]
    fn test_deny_invalid_url_publish() {
        let mut input = "n".as_bytes();
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("invalid-url".to_string()),
            &mut output,
            &mut input,
            true,
        );

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("You cancelled a subgraph publish due to an invalid routing url."));
        assert!(input.is_empty());
        assert!(std::str::from_utf8(&output).unwrap().contains("is not a valid routing URL. Continuing the publish will make this subgraph unreachable by your supergraph. Would you still like to publish?"));
    }

    #[test]
    fn test_invalid_scheme() {
        let mut input = "y".as_bytes();
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("ftp://invalid-scheme".to_string()),
            &mut output,
            &mut input,
            true,
        );

        assert!(result.is_ok());
        assert!(input.is_empty());
        assert!(std::str::from_utf8(&output).unwrap().contains(
            "is not a valid routing URL. The `ftp` protocol is not supported by the router. Valid protocols are `http` and `https`."
        ));
    }

    #[test]
    fn test_localhost_tty() {
        let mut input = "y".as_bytes();
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("http://localhost:8000".to_string()),
            &mut output,
            &mut input,
            true,
        );

        assert!(result.is_ok());
        assert!(input.is_empty());
        assert!(std::str::from_utf8(&output).unwrap().contains(
            "The host `localhost` is not routable via the public internet. Continuing the publish will make this subgraph reachable in local environments only."
        ));
    }

    #[test]
    fn test_localhost_no_tty() {
        let mut input: &[u8] = &[];
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("http://localhost:8000".to_string()),
            &mut output,
            &mut input,
            false,
        );

        assert!(result.is_ok());
        assert!(input.is_empty());
        assert!(std::str::from_utf8(&output).unwrap().contains(
            "The host `localhost` is not routable via the public internet. Continuing the publish will make this subgraph reachable in local environments only."
        ));
    }

    #[test]
    fn test_invalid_url_no_tty() {
        let mut input: &[u8] = &[];
        let mut output: Vec<u8> = Vec::new();
        let result = Publish::handle_maybe_invalid_routing_url(
            &Some("invalid-url".to_string()),
            &mut output,
            &mut input,
            false,
        );

        assert!(result.is_err());
        assert!(input.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("is not a valid routing URL."));
    }
}
