use crate::luau::{LuauObjectRegistryID, bridge::ObjectRegistryType};

/// Represents an unknown value in Luau that wasn't found in the object registry
pub struct UnknownHostLuauValue {
    id: LuauObjectRegistryID,
    typ: ObjectRegistryType,
    err: mluau::Error,
}

impl UnknownHostLuauValue {
    /// Creates a new UnknownHostLuauValue with the specified ID and type
    pub fn new(id: LuauObjectRegistryID, typ: ObjectRegistryType, err: mluau::Error) -> Self {
        if cfg!(feature = "debug_message_print_enabled") {
            println!("UnknownHostLuauValue created of type {} with ID {}: {}", typ.type_name(), id.objid(), err);
        }
        Self { id, typ, err }
    }
}

impl mluau::UserData for UnknownHostLuauValue {
    fn add_fields<F: mluau::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("id", |_, this| Ok(this.id.objid()));
        fields.add_field_method_get("typ", |_, this| Ok(this.typ.type_name()));
        fields.add_field_method_get("error", |_, this| {
            Ok(format!("{}", this.err))
        });
    }
}