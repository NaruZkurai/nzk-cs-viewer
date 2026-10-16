namespace Fixture {
    public static partial class Interp {
        public static string M(string s) {
            string x = $"{(s ?? "//")}tail";
            string a = @"a // b";
            string b = @"a */ b";
            string c = @"a /* b";
            return x + a + b + c;
        }
    }
}
