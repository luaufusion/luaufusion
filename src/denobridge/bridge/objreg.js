/** v8-side object registry 
 * 
 * Given a v8 object, returns a ID for it, or creates a new ID if it doesn't exist
*/
export class V8ObjectRegistry {
    // Reverse mapping of object to id for quick lookup
    #objToId = new WeakMap();
    // Objects stored as [id] = {obj: obj, refcount: n}
    #idToObj = new Map();
    #lastid = 1;

    // Adds an object to the registry, returning its ID
    //
    // Increments refcount if already present
    add(obj) {
        let potid = this.#objToId.get(obj);
        if(potid !== undefined) {
            // Increment refcount
            let entry = this.#idToObj.get(potid);
            entry.refcount++;
            return potid;
        }

        if(this.#lastid >= Number.MAX_SAFE_INTEGER) {
            throw new Error("Object registry full");
        }

        this.#lastid++;
        let id = this.#lastid;
        this.#objToId.set(obj, id);
        this.#idToObj.set(id, {obj: obj, refcount: 1});
        return id;
    }

    // Retrieves an object by its ID
    //
    // Does not modify refcount
    get(id) {
        let entry = this.#idToObj.get(id);
        if (entry) {
            return entry.obj;
        }
        return undefined;
    }

    // Decrements refcount of object by ID, removing it if refcount reaches 0
    drop(id) {
        let entry = this.#idToObj.get(id);
        if(entry) {
            entry.refcount--;
            if(entry.refcount <= 0) {
                this.#idToObj.delete(id);
                this.#objToId.delete(entry.obj);
            }
        }
    }
}
