--- Internal object registry code for Parallel Luau client side
---
--- Given a Luau object, returns a ID for it, or creates a new ID if it doesn't exist

local objreg = {
    --- Reverse mapping of object to id for quick lookup
    objToId = {},
    --- Objects stored as [id] = obj
    idToObj = {},
    --- Current highest ID
    lastid = 0,
}

local MAX_SIZE = 2^53 - 1
local function add(obj)
    -- Check if object already exists
    local potid = objreg.objToId[obj]
    if potid then
        return potid
    end

    -- Create new ID
    if objreg.lastid >= MAX_SIZE then
        error("[Parallel Luau] Object registry full") -- Should never realistically happen
    end
    
    objreg.lastid += 1
    local id = objreg.lastid
    objreg.objToId[obj] = id
    objreg.idToObj[id] = obj
    return id
end

local function get(id)
    return objreg.idToObj[id] -- Returns either the obj or nil if not found
end

local function remove(id)
    local entry = objreg.idToObj[id]
    if entry then
        objreg.idToObj[id] = nil
        objreg.objToId[entry] = nil
    end
end

return {
    objreg = objreg,
    add = add,
    get = get,
    remove = remove,
}