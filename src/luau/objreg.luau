--- Internal object registry code
---
--- Given a Luau object, returns a ID for it, or creates a new ID if it doesn't exist

local objreg = {
    --- Reverse mapping of object to id for quick lookup
    objToId = {},
    --- Objects stored as [id] = {obj=obj, refcount=refcount}
    idToObj = {},
    --- Current highest ID
    lastid = 0,
}

local MAX_SIZE = 2^53 - 1
local function add(obj)
    -- Check if object already exists
    if objreg.objToId[obj] then
        local id = objreg.objToId[obj]
        local ref = objreg.idToObj[id]
        ref.refcount += 1
        return id
    end

    -- Create new ID
    if objreg.lastid >= MAX_SIZE then
        error("Object registry full") -- Should never realistically happen
    end
    
    objreg.lastid += 1
    local id = objreg.lastid
    objreg.objToId[obj] = id
    objreg.idToObj[id] = {obj = obj, refcount = 1}
    return id
end

local function get(id)
    local entry = objreg.idToObj[id]
    if entry then
        return entry.obj
    end
    return nil
end

local function remove(id)
    local entry = objreg.idToObj[id]
    if entry then
        entry.refcount -= 1
        -- If no more references, remove the object
        if entry.refcount <= 0 then
            objreg.idToObj[id] = nil
            objreg.objToId[entry.obj] = nil
        end
    end
end

return {
    objreg = objreg,
    add = add,
    get = get,
    remove = remove,
}