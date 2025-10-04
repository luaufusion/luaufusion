/** v8-side object registry 
 * 
 * Given a v8 object, returns a ID for it, or creates a new ID if it doesn't exist
*/
export class V8ObjectRegistry {
    // Reverse mapping of object to id for quick lookup
    #objToId = new Map();
    // Objects stored as [id] = {obj=obj, refcount=refcount}
    #idToObj = new Map();
    #lastid = 1;

    add(obj) {
        if(this.#objToId.has(obj)) {
            let id = this.#objToId.get(obj);
            let entry = this.#idToObj.get(id);
            entry.refcount++;
            return id;
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

    get(id) {
        let entry = this.#idToObj.get(id);
        if(entry) {
            return entry.obj;
        }
        return null;
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
