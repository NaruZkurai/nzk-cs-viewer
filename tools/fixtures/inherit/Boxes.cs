// Type-hierarchy fixture: generic base class and a concrete derived class.
namespace Inherit.Fixture
{
    public abstract class GenericBox<T>
    {
        public abstract T Unwrap();
    }

    public class IntBox : GenericBox<int>
    {
        public override int Unwrap()
        {
            return 1;
        }
    }
}
