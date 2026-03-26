use crate::plugin::ToolPolicy;
use crate::session::model::{
    Session, ToolPermissionAction, ToolPermissionRule, ToolPermissionSubject, ToolSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    Once,
    Always,
    Deny,
}

pub fn evaluate_tool_permission(session: &Session, policy: &ToolPolicy) -> PermissionDecision {
    match session.remembered_tool_permission_action(&policy.tool_name, &policy.tool_source) {
        Some(ToolPermissionAction::Allow) => PermissionDecision::Allow,
        Some(ToolPermissionAction::Deny) => PermissionDecision::Deny,
        Some(ToolPermissionAction::Ask) => PermissionDecision::Ask,
        None => match policy.default_action {
            ToolPermissionAction::Allow => PermissionDecision::Allow,
            ToolPermissionAction::Ask => PermissionDecision::Ask,
            ToolPermissionAction::Deny => PermissionDecision::Deny,
        },
    }
}

pub fn remember_reply(session: &mut Session, policy: &ToolPolicy, reply: PermissionReply) {
    let action = match reply {
        PermissionReply::Always => Some(ToolPermissionAction::Allow),
        PermissionReply::Deny => Some(ToolPermissionAction::Deny),
        PermissionReply::Once => None,
    };

    if let Some(action) = action {
        session.remember_tool_permission_rule(ToolPermissionRule {
            subject: ToolPermissionSubject::from_tool(
                policy.tool_name.clone(),
                &policy.tool_source,
            ),
            action,
        });
    }
}

pub fn denial_message(tool_name: &str) -> String {
    format!("Permission denied for tool '{tool_name}' by user")
}

pub fn tool_denied_by_policy_message(tool_name: &str) -> String {
    format!("Tool '{tool_name}' is denied by session permission policy")
}

pub fn can_remember_reply(policy: &ToolPolicy, reply: PermissionReply) -> bool {
    matches!(reply, PermissionReply::Always | PermissionReply::Deny) && policy.rememberable
}

pub fn tool_subject(tool_name: &str, tool_source: &ToolSource) -> ToolPermissionSubject {
    ToolPermissionSubject::from_tool(tool_name.to_string(), tool_source)
}

#[cfg(test)]
mod tests {
    use crate::plugin::{ToolPolicy, ToolPolicyOrigin};
    use crate::session::model::{Session, ToolPermissionAction, ToolSource};

    use super::{
        PermissionDecision, PermissionReply, can_remember_reply, evaluate_tool_permission,
        remember_reply,
    };

    fn built_in_policy(default_action: ToolPermissionAction) -> ToolPolicy {
        ToolPolicy {
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            default_action,
            rememberable: true,
            origin: ToolPolicyOrigin::BuiltInDefault,
        }
    }

    #[test]
    fn defaults_to_tool_policy_when_no_session_rule_exists() {
        let session = Session::new("permission test");

        assert_eq!(
            evaluate_tool_permission(&session, &built_in_policy(ToolPermissionAction::Ask)),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate_tool_permission(&session, &built_in_policy(ToolPermissionAction::Allow)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn remembered_rule_overrides_default_policy() {
        let mut session = Session::new("permission test");
        let policy = built_in_policy(ToolPermissionAction::Ask);

        remember_reply(&mut session, &policy, PermissionReply::Always);

        assert_eq!(
            evaluate_tool_permission(&session, &policy),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn non_rememberable_policy_rejects_persistent_reply() {
        let policy = ToolPolicy {
            tool_name: "task".to_string(),
            tool_source: ToolSource::BuiltIn,
            default_action: ToolPermissionAction::Ask,
            rememberable: false,
            origin: ToolPolicyOrigin::BuiltInDefault,
        };

        assert!(!can_remember_reply(&policy, PermissionReply::Always));
        assert!(!can_remember_reply(&policy, PermissionReply::Deny));
        assert!(!can_remember_reply(&policy, PermissionReply::Once));
    }
}
