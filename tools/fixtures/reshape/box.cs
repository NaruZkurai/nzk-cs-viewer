using System;

namespace Fixture {
    public static partial class Box {
        // a box
        /// Adds two numbers.
        public static int Add(int a, int b) {
            return Counter.Sum(a, b);
        }
    }
}
