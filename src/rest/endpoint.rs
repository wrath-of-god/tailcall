use std::collections::BTreeMap;

use async_graphql::parser::types::{DocumentOperations, ExecutableDocument};
use async_graphql::Variables;
use derive_setters::Setters;

use super::directive::Rest;
use super::partial_request::PartialRequest;
use super::path::Path;
use super::query_params::QueryParams;
use super::type_map::TypeMap;
use super::Request;
use crate::directive::DirectiveCodec;
use crate::http::Method;

/// An executable Http Endpoint created from a GraphQL query
#[derive(Debug, Setters, Clone)]
pub struct Endpoint {
    method: Method,
    path: Path,

    // Can use persisted queries for better performance
    query_params: QueryParams,
    body: Option<String>,
    doc: ExecutableDocument,
}

/// Creates a Rest instance from @rest directive

impl Endpoint {
    pub fn try_new(operations: &str) -> anyhow::Result<Vec<Self>> {
        let doc = async_graphql::parser::parse_query(operations)?;
        let mut endpoints = Vec::new();

        for (_, op) in doc.operations.iter() {
            let type_map = TypeMap::new(
                op.node
                    .variable_definitions
                    .iter()
                    .map(|pos| {
                        (
                            pos.node.name.node.to_string(),
                            pos.node.var_type.node.clone(),
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
            );

            let rest = op.node.directives.iter().find_map(|d| {
                if d.node.name.node == Rest::directive_name() {
                    let rest = Rest::try_from(&d.node);
                    Some(rest)
                } else {
                    None
                }
            });

            if let Some(rest) = rest {
                let rest = rest?;
                let endpoint = Self {
                    method: rest.method.unwrap_or_default(),
                    path: Path::parse(&type_map, &rest.path)?,
                    query_params: QueryParams::try_from_map(&type_map, rest.query)?,
                    body: rest.body,
                    doc: ExecutableDocument {
                        operations: DocumentOperations::Single(op.clone()),
                        fragments: doc.fragments.clone(),
                    },
                };
                endpoints.push(endpoint);
            }
        }

        Ok(endpoints)
    }

    pub fn matches<'a>(&'a self, request: &Request) -> Option<PartialRequest<'a>> {
        let query_params = request
            .uri()
            .query()
            .map(|query| serde_urlencoded::from_str(query).unwrap_or_else(|_| BTreeMap::new()))
            .unwrap_or_default();

        let mut variables = Variables::default();

        // Method
        if self.method.clone().to_hyper() != request.method() {
            return None;
        }

        // Path
        let path = self.path.matches(request.uri().path())?;

        // Query
        let query = self.query_params.matches(query_params)?;

        // FIXME: Too much cloning is happening via merge_variables
        variables = merge_variables(variables, path);
        variables = merge_variables(variables, query);

        Some(PartialRequest { body: self.body.as_ref(), doc: &self.doc, variables })
    }
}

fn merge_variables(a: Variables, b: Variables) -> Variables {
    let mut variables = Variables::default();

    for (k, v) in a.iter() {
        variables.insert(k.clone(), v.clone());
    }

    for (k, v) in b.iter() {
        variables.insert(k.clone(), v.clone());
    }

    variables
}

#[cfg(test)]
mod tests {
    use async_graphql::parser::types::Directive;
    use maplit::btreemap;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::rest::path::Segment;
    use crate::rest::typed_variables::TypedVariable;

    const TEST_QUERY: &str = r#"
        query ($a: Int, $b: String, $c: Boolean, $d: Float, $v: String)
          @rest(method: "post", path: "/foo/$a", query: {b: $b, c: $c, d: $d}, body: $v) {
            value
          }
        "#;

    const MULTIPLE_TEST_QUERY: &str = r#"        
        query q1 ($a: Int)
          @rest(method: "post", path: "/foo/$a") {
            value
          }


        query q2 ($a: Int)
          @rest(method: "post", path: "/bar/$a") {
            value
          }
        "#;

    impl Path {
        fn new(segments: Vec<Segment>) -> Self {
            Self { segments }
        }
    }
    fn test_directive() -> Directive {
        async_graphql::parser::parse_query(TEST_QUERY)
            .unwrap()
            .operations
            .iter()
            .next()
            .unwrap()
            .1
            .node
            .directives
            .first()
            .unwrap()
            .node
            .clone()
    }

    #[test]
    fn test_rest() {
        let directive = test_directive();
        let actual = Rest::try_from(&directive).unwrap();
        let expected = Rest::default()
            .path("/foo/$a".to_string())
            .method(Some(Method::POST))
            .query(
                btreemap! { "b".to_string() => "b".to_string(), "c".to_string() => "c".to_string(), "d".to_string() => "d".to_string() },
            )
            .body(Some("v".to_string()));

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_endpoint() {
        let endpoint = &Endpoint::try_new(TEST_QUERY).unwrap()[0];
        assert_eq!(endpoint.method, Method::POST);
        assert_eq!(
            endpoint.path,
            Path::new(vec![
                Segment::lit("foo"),
                Segment::param(TypedVariable::int("a")),
            ])
        );
        assert_eq!(
            endpoint.query_params,
            QueryParams::from(vec![
                ("b", TypedVariable::string("b")),
                ("c", TypedVariable::boolean("c")),
                ("d", TypedVariable::float("d"))
            ])
        );
        assert_eq!(endpoint.body, Some("v".to_string()));
    }

    #[test]
    fn test_multiple_queries() {
        let endpoints = Endpoint::try_new(MULTIPLE_TEST_QUERY).unwrap();
        assert_eq!(endpoints.len(), 2);
    }

    mod matches {
        use std::str::FromStr;

        use async_graphql::Variables;
        use async_graphql_value::{ConstValue, Name};
        use hyper::{Body, Method, Request, Uri, Version};
        use maplit::btreemap;
        use pretty_assertions::assert_eq;

        use crate::rest::endpoint::tests::TEST_QUERY;
        use crate::rest::endpoint::Endpoint;

        fn test_request(method: Method, uri: &str) -> anyhow::Result<hyper::Request<Body>> {
            Ok(Request::builder()
                .method(method)
                .uri(Uri::from_str(uri)?)
                .version(Version::HTTP_11)
                .body(Body::empty())?)
        }

        fn test_matches(query: &str, method: Method, uri: &str) -> Option<Variables> {
            let endpoint = &mut Endpoint::try_new(query).unwrap()[0];
            let request = test_request(method, uri).unwrap();

            endpoint.matches(&request).map(|req| req.variables)
        }

        #[test]
        fn test_valid() {
            let actual = test_matches(
                TEST_QUERY,
                Method::POST,
                "http://localhost:8080/foo/1?b=b&c=true&d=1.25",
            );
            let expected = &btreemap! {
                Name::new("a") => ConstValue::from(1),
                Name::new("b") => ConstValue::from("b"),
                Name::new("c") => ConstValue::from(true),
                Name::new("d") => ConstValue::from(1.25),
            };
            pretty_assertions::assert_eq!(actual.as_deref(), Some(expected))
        }

        #[test]
        fn test_path_not_match() {
            let actual = test_matches(
                TEST_QUERY,
                Method::POST,
                "http://localhost:8080/bar/1?b=b&c=true",
            );

            assert_eq!(actual, None);
            let actual = test_matches(
                TEST_QUERY,
                Method::POST,
                "http://localhost:8080/foo/1/nested?b=b&c=true",
            );

            assert_eq!(actual, None);
        }

        #[test]
        fn test_invalid_url_param() {
            let actual = test_matches(
                TEST_QUERY,
                Method::POST,
                "http://localhost:8080/foo/a?b=b&c=true",
            );
            pretty_assertions::assert_eq!(actual, None)
        }

        #[test]
        fn test_query_params_optional() {
            let actual = test_matches(TEST_QUERY, Method::POST, "http://localhost:8080/foo/1");
            let expected = &btreemap! {
                Name::new("a") => ConstValue::from(1),
            };
            pretty_assertions::assert_eq!(actual.as_deref(), Some(expected));

            let actual = test_matches(TEST_QUERY, Method::POST, "http://localhost:8080/foo/1/?b=b");
            let expected = &btreemap! {
                Name::new("a") => ConstValue::from(1),
                Name::new("b") => ConstValue::from("b"),
            };
            pretty_assertions::assert_eq!(actual.as_deref(), Some(expected))
        }

        #[test]
        fn test_invalid_query_param() {
            let actual = test_matches(
                TEST_QUERY,
                Method::POST,
                "http://localhost:8080/foo/1?b=b&c=c",
            );
            assert_eq!(actual, None)
        }

        #[test]
        fn test_method_not_match() {
            let actual = test_matches(
                TEST_QUERY,
                Method::GET,
                "http://localhost:8080/foo/1?b=b&c=true",
            );
            assert_eq!(actual, None)
        }
    }
}
