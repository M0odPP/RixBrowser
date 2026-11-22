/*---
description: Class extends, super constructor/method calls, getters, static blocks, and private fields
---*/
class Animal {
    #legs;
    static kingdom = "Animalia";
    constructor(name, legs) {
        this.name = name;
        this.#legs = legs;
    }
    describe() { return this.name + ":" + this.#legs; }
    hasLegsField() { return #legs in this; }
}
class Dog extends Animal {
    constructor(name) { super(name, 4); }
    describe() { return "Dog(" + super.describe() + ")"; }
}
const d = new Dog("Rex");
assert.sameValue(d instanceof Dog, true);
assert.sameValue(d instanceof Animal, true);
assert.sameValue(d.describe(), "Dog(Rex:4)");
assert.sameValue(d.hasLegsField(), true);
assert.sameValue(Dog.kingdom, "Animalia");
