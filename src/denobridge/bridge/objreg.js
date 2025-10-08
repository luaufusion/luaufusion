/** v8-side object registry 
 * 
 * Given a v8 object, returns a ID for it, or creates a new ID if it doesn't exist
*/
export class V8ObjectRegistry {
    // Reverse mapping of object to id for quick lookup
    #objToId = new Map();
    // Objects stored as [id] = obj
    #idToObj = new Map();
    #lastid = 1;

    add(obj) {
        let potid = this.#objToId.get(obj);
        if(this.#objToId.has(obj)) {
            return potid;
        }

        if(this.#lastid >= Number.MAX_SAFE_INTEGER) {
            throw new Error("Object registry full");
        }

        this.#lastid++;
        let id = this.#lastid;
        this.#objToId.set(obj, id);
        this.#idToObj.set(id, obj);
        return id;
    }

    get(id) {
        return this.#idToObj.get(id);
    }

    remove(id) {
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
