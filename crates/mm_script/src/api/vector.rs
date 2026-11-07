//! `Vector3`: an immutable 3D vector, as in Roblox.

use mlua::{FromLua, Lua, MetaMethod, MultiValue, UserData, UserDataFields, UserDataMethods, Value};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Vector3(pub [f64; 3]);

impl Vector3 {
    fn map2(self, other: Self, f: impl Fn(f64, f64) -> f64) -> Self {
        Self([0, 1, 2].map(|i| f(self.0[i], other.0[i])))
    }

    fn magnitude(self) -> f64 {
        self.0.iter().map(|c| c * c).sum::<f64>().sqrt()
    }

    /// The block containing this point.
    pub fn block(self) -> [i32; 3] {
        self.0.map(|c| c.floor() as i32)
    }
}

impl FromLua for Vector3 {
    fn from_lua(value: Value, _: &Lua) -> mlua::Result<Self> {
        match &value {
            Value::UserData(ud) => Ok(*ud.borrow::<Self>()?),
            _ => Err(mlua::Error::FromLuaConversionError {
                from: value.type_name(),
                to: "Vector3".into(),
                message: None,
            }),
        }
    }
}

/// Either a `Vector3` or three numbers.
pub fn point(args: &MultiValue, lua: &Lua) -> mlua::Result<[f64; 3]> {
    match args.len() {
        1 => Ok(Vector3::from_lua(args[0].clone(), lua)?.0),
        3 => {
            let mut p = [0.0; 3];
            for (i, v) in args.iter().enumerate() {
                p[i] = f64::from_lua(v.clone(), lua)?;
            }
            Ok(p)
        }
        _ => Err(mlua::Error::runtime("expected a Vector3 or x, y, z")),
    }
}

fn scale(lua: &Lua, a: Value, b: Value, f: impl Fn(f64, f64) -> f64) -> mlua::Result<Vector3> {
    match (&a, &b) {
        (Value::UserData(_), Value::UserData(_)) => {
            Ok(Vector3::from_lua(a, lua)?.map2(Vector3::from_lua(b, lua)?, f))
        }
        (Value::UserData(_), _) => {
            let (v, n) = (Vector3::from_lua(a, lua)?, f64::from_lua(b, lua)?);
            Ok(Vector3(v.0.map(|c| f(c, n))))
        }
        _ => {
            let (n, v) = (f64::from_lua(a, lua)?, Vector3::from_lua(b, lua)?);
            Ok(Vector3(v.0.map(|c| f(n, c))))
        }
    }
}

impl UserData for Vector3 {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("X", |_, v| Ok(v.0[0]));
        fields.add_field_method_get("Y", |_, v| Ok(v.0[1]));
        fields.add_field_method_get("Z", |_, v| Ok(v.0[2]));
        fields.add_field_method_get("Magnitude", |_, v| Ok(v.magnitude()));
        fields.add_field_method_get("Unit", |_, v| {
            let m = v.magnitude();
            Ok(if m == 0.0 { *v } else { Vector3(v.0.map(|c| c / m)) })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Dot", |_, a, b: Vector3| Ok((0..3).map(|i| a.0[i] * b.0[i]).sum::<f64>()));
        methods.add_method("Cross", |_, a, b: Vector3| {
            let (a, b) = (a.0, b.0);
            Ok(Vector3([a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]))
        });
        methods.add_method("Lerp", |_, a, (b, t): (Vector3, f64)| Ok(a.map2(b, |x, y| x + (y - x) * t)));
        methods.add_method("Floor", |_, v, ()| Ok(Vector3(v.0.map(f64::floor))));
        methods.add_meta_method(MetaMethod::Eq, |_, a, b: Vector3| Ok(*a == b));
        methods.add_meta_method(MetaMethod::Unm, |_, v, ()| Ok(Vector3(v.0.map(|c| -c))));
        methods.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(format!("{}, {}, {}", v.0[0], v.0[1], v.0[2])));
        methods.add_meta_function(MetaMethod::Add, |_, (a, b): (Vector3, Vector3)| Ok(a.map2(b, |x, y| x + y)));
        methods.add_meta_function(MetaMethod::Sub, |_, (a, b): (Vector3, Vector3)| Ok(a.map2(b, |x, y| x - y)));
        methods.add_meta_function(MetaMethod::Mul, |lua, (a, b): (Value, Value)| scale(lua, a, b, |x, y| x * y));
        methods.add_meta_function(MetaMethod::Div, |lua, (a, b): (Value, Value)| scale(lua, a, b, |x, y| x / y));
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let class = lua.create_table()?;
    class.set(
        "new",
        lua.create_function(|_, (x, y, z): (Option<f64>, Option<f64>, Option<f64>)| {
            Ok(Vector3([x.unwrap_or(0.0), y.unwrap_or(0.0), z.unwrap_or(0.0)]))
        })?,
    )?;
    class.set("zero", Vector3([0.0; 3]))?;
    class.set("one", Vector3([1.0; 3]))?;
    lua.globals().set("Vector3", class)
}
