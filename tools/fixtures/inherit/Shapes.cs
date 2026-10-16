// Type-hierarchy fixture: abstract base, override, interface extension,
// and a class that implements two interfaces.
namespace Inherit.Fixture
{
    public interface INamed
    {
        string Name { get; }
    }

    // An interface extending another interface.
    public interface IShape2D : INamed
    {
        double Area();
    }

    public interface IRenderable
    {
        void Render();
    }

    public abstract class Shape
    {
        public abstract double Area();

        public virtual string Describe()
        {
            return "shape";
        }
    }

    // Derived class: inherits Shape, implements two interfaces directly,
    // and overrides the abstract Shape.Area.
    public class Circle : Shape, IShape2D, IRenderable
    {
        public Circle(string name)
        {
            Name = name;
        }

        public string Name { get; }

        public override double Area()
        {
            return 3.14;
        }

        public override string Describe()
        {
            return "circle " + Name;
        }

        public void Render()
        {
        }
    }

    // Second-level derivation: gives the base chain a transitive hop.
    public class ColoredCircle : Circle
    {
        public ColoredCircle(string name)
            : base(name)
        {
        }
    }
}
