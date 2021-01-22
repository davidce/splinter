// Copyright 2018-2021 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::actix_web::{error::BlockingError, web, Error, HttpRequest, HttpResponse};
use crate::futures::{Future, IntoFuture};
use crate::protocol;
use crate::rest_api::{
    actix_web_1::{Method, ProtocolVersionRangeGuard, Resource},
    auth::rbac::{
        rest_api::{
            resources::{
                assignments::{AssignmentResponse, ListAssignmentsResponse},
                PagingQuery,
            },
            RBAC_READ_PERMISSION,
        },
        store::RoleBasedAuthorizationStore,
    },
    paging::get_response_paging_info,
    ErrorResponse,
};

use super::error::SendableRoleBasedAuthorizationStoreError;

pub fn make_assignments_resource(
    role_based_auth_store: Box<dyn RoleBasedAuthorizationStore>,
) -> Resource {
    let list_store = role_based_auth_store;
    Resource::build("/authorization/assignments")
        .add_request_guard(ProtocolVersionRangeGuard::new(
            protocol::AUTHORIZATION_RBAC_ASSIGNMENTS_MIN,
            protocol::AUTHORIZATION_PROTOCOL_VERSION,
        ))
        .add_method(Method::Get, RBAC_READ_PERMISSION, move |r, _| {
            list_assignments(r, web::Data::new(list_store.clone()))
        })
}

fn list_assignments(
    req: HttpRequest,
    role_based_auth_store: web::Data<Box<dyn RoleBasedAuthorizationStore>>,
) -> Box<dyn Future<Item = HttpResponse, Error = Error>> {
    let web::Query(paging_query): web::Query<PagingQuery> =
        match web::Query::from_query(req.query_string()) {
            Ok(paging_query) => paging_query,
            Err(_) => {
                return Box::new(
                    HttpResponse::BadRequest()
                        .json(ErrorResponse::bad_request("Invalid query"))
                        .into_future(),
                )
            }
        };

    let link = format!("{}?", req.uri().path());

    Box::new(
        web::block(move || {
            let assignments = role_based_auth_store
                .list_assignments()
                .map_err(SendableRoleBasedAuthorizationStoreError::from)?;

            let total = assignments.len();
            let assignments = assignments
                .skip(paging_query.offset)
                .take(paging_query.limit)
                .collect::<Vec<_>>();

            Ok((assignments, link, paging_query, total))
        })
        .then(
            |res: Result<_, BlockingError<SendableRoleBasedAuthorizationStoreError>>| match res {
                Ok((assignments, link, paging_query, total)) => {
                    Ok(HttpResponse::Ok().json(ListAssignmentsResponse {
                        data: assignments.iter().map(AssignmentResponse::from).collect(),
                        paging: get_response_paging_info(
                            Some(paging_query.limit),
                            Some(paging_query.offset),
                            &link,
                            total,
                        ),
                    }))
                }
                Err(err) => {
                    error!("Unable to list assignments: {}", err);
                    Ok(HttpResponse::InternalServerError().json(ErrorResponse::internal_error()))
                }
            },
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use reqwest::{blocking::Client, StatusCode, Url};
    use serde_json::{to_value, Value as JsonValue};

    use crate::error::{ConstraintViolationError, ConstraintViolationType};
    use crate::rest_api::auth::rbac::store::{
        Assignment, AssignmentBuilder, Identity, Role, RoleBasedAuthorizationStoreError,
        RoleBuilder,
    };
    use crate::rest_api::{
        actix_web_1::{RestApiBuilder, RestApiShutdownHandle},
        paging::Paging,
    };

    macro_rules! get_in {
        ($val:expr, $keys:expr, $as:ident) => {{
            let mut result = Some(&$val);
            for k in $keys {
                result = result.and_then(|next| next.get(k));
            }

            result.and_then(|last_val| last_val.$as())
        }};
    }

    /// Tests a GET /authorization/assignments request which returns the set of assignments.
    /// 1. Adds two roles to the store
    /// 2. Add two assignments, one for a key identity, one for a user identity and assign both the
    ///    roles to each
    /// 3. Perform a GET against /authorization/assignments
    /// 4. Verify that it includes both assignments
    #[test]
    fn test_list_assignments_ok() {
        let role_based_auth_store = MemRoleBasedAuthorizationStore::default();

        let role = RoleBuilder::new()
            .with_id("role-1".into())
            .with_display_name("Test Role 1".into())
            .with_permissions(vec!["a".to_string(), "b".to_string(), "c".to_string()])
            .build()
            .expect("Unable to build role");

        role_based_auth_store
            .add_role(role)
            .expect("Unable to add role");

        let role = RoleBuilder::new()
            .with_id("role-2".into())
            .with_display_name("Test Role 2".into())
            .with_permissions(vec!["x".to_string(), "y".to_string(), "z".to_string()])
            .build()
            .expect("Unable to build role");

        role_based_auth_store
            .add_role(role)
            .expect("Unable to add role");

        let assignment = AssignmentBuilder::new()
            .with_identity(Identity::Key("x".into()))
            .with_roles(vec!["role-1".to_string(), "role-2".to_string()])
            .build()
            .expect("Unable to build assignment");

        role_based_auth_store
            .add_assignment(assignment)
            .expect("Unable to add assignment");

        let assignment = AssignmentBuilder::new()
            .with_identity(Identity::User("y".into()))
            .with_roles(vec!["role-1".to_string(), "role-2".to_string()])
            .build()
            .expect("Unable to build assignment");

        role_based_auth_store
            .add_assignment(assignment)
            .expect("Unable to add assignment");

        let (shutdown_handle, join_handle, bind_url) =
            run_rest_api_on_open_port(vec![make_assignments_resource(Box::new(
                role_based_auth_store,
            ))]);

        let url = Url::parse(&format!("http://{}/authorization/assignments", bind_url))
            .expect("Failed to parse URL");

        let resp = Client::new()
            .get(url)
            .header(
                "SplinterProtocolVersion",
                protocol::AUTHORIZATION_PROTOCOL_VERSION,
            )
            .send()
            .expect("Failed to perform request");

        assert_eq!(resp.status(), StatusCode::OK);
        let body: JsonValue = resp.json().expect("Failed to deserialize body");

        let json_assignments = get_in!(body, &["data"], as_array)
            .expect("data field is not an array")
            .to_vec();

        assert_eq!(2, json_assignments.len());

        assert_eq!(
            &json!({
                "identity": "x",
                "identity_type": "key",
                "roles": ["role-1", "role-2"],
            }),
            json_assignments.get(0).expect("no first item")
        );

        assert_eq!(
            &json!({
                "identity": "y",
                "identity_type": "user",
                "roles": ["role-1", "role-2"],
            }),
            json_assignments.get(1).expect("no second item")
        );

        assert_eq!(
            body.get("paging").expect("no paging field in response"),
            &to_value(create_test_paging_response(
                0,
                100,
                0,
                0,
                0,
                2,
                "/authorization/assignments?"
            ))
            .expect("failed to convert expected paging")
        );

        shutdown_handle
            .shutdown()
            .expect("Unable to shutdown rest api");
        join_handle.join().expect("Unable to join rest api thread");
    }

    /// Tests a GET /authorization/assignments request which returns the set of assignments.
    /// 1. Add two roles to the store
    /// 2. Add 101 assignments which include both roles
    /// 3. Perform a GET against /authorization/assignments
    /// 4. Verify that 100 elements are returned and that there is a next URL
    /// 5. Perform a GET request against the next URL
    /// 6. Verify that the 101st assignment is in the list
    #[test]
    fn test_list_assignments_paging_ok() {
        let role_based_auth_store = MemRoleBasedAuthorizationStore::default();

        let role = RoleBuilder::new()
            .with_id("role-1".into())
            .with_display_name("Test Role 1".into())
            .with_permissions(vec!["a".to_string(), "b".to_string(), "c".to_string()])
            .build()
            .expect("Unable to build role");

        role_based_auth_store
            .add_role(role)
            .expect("Unable to add role");

        let role = RoleBuilder::new()
            .with_id("role-2".into())
            .with_display_name("Test Role 2".into())
            .with_permissions(vec!["x".to_string(), "y".to_string(), "z".to_string()])
            .build()
            .expect("Unable to build role");

        role_based_auth_store
            .add_role(role)
            .expect("Unable to add role");

        for i in 0..101 {
            let assignment = AssignmentBuilder::new()
                .with_identity(Identity::User(format!("id-{:0>3}", i)))
                .with_roles(vec!["role-1".to_string(), "role-2".to_string()])
                .build()
                .expect("Unable to build assignment");

            role_based_auth_store
                .add_assignment(assignment)
                .expect("Unable to add assignment");
        }

        let (shutdown_handle, join_handle, bind_url) =
            run_rest_api_on_open_port(vec![make_assignments_resource(Box::new(
                role_based_auth_store,
            ))]);

        let url = Url::parse(&format!("http://{}/authorization/assignments", bind_url))
            .expect("Failed to parse URL");

        let resp = Client::new()
            .get(url)
            .header(
                "SplinterProtocolVersion",
                protocol::AUTHORIZATION_PROTOCOL_VERSION,
            )
            .send()
            .expect("Failed to perform request");

        assert_eq!(resp.status(), StatusCode::OK);
        let body: JsonValue = resp.json().expect("Failed to deserialize body");

        let json_assignments = get_in!(body, &["data"], as_array)
            .expect("data field is not an array")
            .to_vec();

        assert_eq!(100, json_assignments.len());

        for i in 0..100 {
            assert_eq!(
                &json!({
                    "identity": format!("id-{:0>3}", i),
                    "identity_type": "user",
                    "roles": ["role-1", "role-2"],
                }),
                json_assignments.get(i).expect("no first item")
            );
        }

        assert_eq!(
            &to_value(create_test_paging_response(
                0,
                100,
                100,
                0,
                100,
                101,
                "/authorization/assignments?"
            ))
            .expect("failed to convert expected paging"),
            body.get("paging").expect("no paging field in response"),
        );

        let next_link = get_in!(body, &["paging", "next"], as_str)
            .expect("paging.next field should be a string");

        let url =
            Url::parse(&format!("http://{}{}", bind_url, next_link)).expect("Failed to parse URL");

        let resp = Client::new()
            .get(url)
            .header(
                "SplinterProtocolVersion",
                protocol::AUTHORIZATION_PROTOCOL_VERSION,
            )
            .send()
            .expect("Failed to perform request");

        assert_eq!(resp.status(), StatusCode::OK);
        let body: JsonValue = resp.json().expect("Failed to deserialize body");

        let json_assignments = body
            .get("data")
            .expect("No data field in response")
            .as_array()
            .expect("data field is not an array")
            .to_vec();
        assert_eq!(1, json_assignments.len());

        assert_eq!(
            &json!({
                "identity": "id-100",
                "identity_type": "user",
                "roles": ["role-1", "role-2"],
            }),
            json_assignments.get(0).expect("no first item")
        );

        shutdown_handle
            .shutdown()
            .expect("Unable to shutdown rest api");
        join_handle.join().expect("Unable to join rest api thread");
    }

    fn run_rest_api_on_open_port(
        resources: Vec<Resource>,
    ) -> (RestApiShutdownHandle, std::thread::JoinHandle<()>, String) {
        #[cfg(not(feature = "https-bind"))]
        let bind = "127.0.0.1:0";
        #[cfg(feature = "https-bind")]
        let bind = crate::rest_api::RestApiBind::Insecure("127.0.0.1:0".into());

        let result = RestApiBuilder::new()
            .with_bind(bind)
            .add_resources(resources.clone())
            .build_insecure()
            .expect("Failed to build REST API")
            .run_insecure();
        match result {
            Ok((shutdown_handle, join_handle)) => {
                let port = shutdown_handle.port_numbers()[0];
                (shutdown_handle, join_handle, format!("127.0.0.1:{}", port))
            }
            Err(err) => panic!("Failed to run REST API: {}", err),
        }
    }

    fn create_test_paging_response(
        offset: usize,
        limit: usize,
        next_offset: usize,
        previous_offset: usize,
        last_offset: usize,
        total: usize,
        link: &str,
    ) -> Paging {
        let base_link = format!("{}limit={}&", link, limit);
        let current_link = format!("{}offset={}", base_link, offset);
        let first_link = format!("{}offset=0", base_link);
        let next_link = format!("{}offset={}", base_link, next_offset);
        let previous_link = format!("{}offset={}", base_link, previous_offset);
        let last_link = format!("{}offset={}", base_link, last_offset);

        Paging {
            current: current_link,
            offset,
            limit,
            total,
            first: first_link,
            prev: previous_link,
            next: next_link,
            last: last_link,
        }
    }

    #[derive(Clone, Default)]
    struct MemRoleBasedAuthorizationStore {
        assignments: Arc<Mutex<BTreeMap<String, Assignment>>>,
    }

    impl RoleBasedAuthorizationStore for MemRoleBasedAuthorizationStore {
        fn get_role(&self, _id: &str) -> Result<Option<Role>, RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn list_roles(
            &self,
        ) -> Result<Box<dyn ExactSizeIterator<Item = Role>>, RoleBasedAuthorizationStoreError>
        {
            unimplemented!()
        }

        fn add_role(&self, _role: Role) -> Result<(), RoleBasedAuthorizationStoreError> {
            Ok(())
        }

        fn update_role(&self, _role: Role) -> Result<(), RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn remove_role(&self, _role_id: &str) -> Result<(), RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn get_assignment(
            &self,
            _identity: &Identity,
        ) -> Result<Option<Assignment>, RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn get_assigned_roles(
            &self,
            _identity: &Identity,
        ) -> Result<Box<dyn ExactSizeIterator<Item = Role>>, RoleBasedAuthorizationStoreError>
        {
            unimplemented!()
        }

        fn list_assignments(
            &self,
        ) -> Result<Box<dyn ExactSizeIterator<Item = Assignment>>, RoleBasedAuthorizationStoreError>
        {
            Ok(Box::new(
                self.assignments
                    .lock()
                    .expect("mem role based authorization store lock was poisoned")
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter(),
            ))
        }

        fn add_assignment(
            &self,
            assignment: Assignment,
        ) -> Result<(), RoleBasedAuthorizationStoreError> {
            let mut assignments = self
                .assignments
                .lock()
                .expect("mem role based authorization store lock was poisoned");

            let key = id_to_string(assignment.identity());

            if !assignments.contains_key(&key) {
                assignments.insert(key, assignment);
                Ok(())
            } else {
                Err(RoleBasedAuthorizationStoreError::ConstraintViolation(
                    ConstraintViolationError::with_violation_type(ConstraintViolationType::Unique),
                ))
            }
        }

        fn update_assignment(
            &self,
            _assignment: Assignment,
        ) -> Result<(), RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn remove_assignment(
            &self,
            _identity: &Identity,
        ) -> Result<(), RoleBasedAuthorizationStoreError> {
            unimplemented!()
        }

        fn clone_box(&self) -> Box<dyn RoleBasedAuthorizationStore> {
            Box::new(self.clone())
        }
    }

    fn id_to_string(identity: &Identity) -> String {
        match identity {
            Identity::Key(key) => format!("key-{}", key),
            Identity::User(user) => format!("user-{}", user),
        }
    }
}
