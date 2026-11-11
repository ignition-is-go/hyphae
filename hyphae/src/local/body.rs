use std::any::Any;

use super::{Builder, Cell, ErasedValue, Error, Id, Result, Runtime, ScopeId, Value};

pub(super) trait Body: Send {
    fn value(&self) -> Option<&dyn Any>;
    fn set(&mut self, _value: ErasedValue) -> Result<bool> {
        Err(Error::NotInput)
    }
    fn prepare(&mut self, _rt: &mut Runtime, _id: Id) -> Result<()> {
        Ok(())
    }
    fn evaluate(&mut self, rt: &Runtime) -> Result<bool>;
    fn scope(&self) -> Option<ScopeId> {
        None
    }
}

type Compute<T> = Box<dyn FnMut(&Runtime) -> Result<T> + Send>;
pub(super) struct Typed<T> {
    pub value: Option<T>,
    pub compute: Option<Compute<T>>,
    pub input: bool,
}
impl<T: Value> Body for Typed<T> {
    fn value(&self) -> Option<&dyn Any> {
        self.value.as_ref().map(|v| -> &dyn Any { v })
    }
    fn set(&mut self, value: ErasedValue) -> Result<bool> {
        if !self.input {
            return Err(Error::NotInput);
        }
        let value = *value.downcast::<T>().map_err(|_| Error::TypeMismatch)?;
        let changed = self.value.as_ref() != Some(&value);
        if changed {
            self.value = Some(value);
        }
        Ok(changed)
    }
    fn evaluate(&mut self, rt: &Runtime) -> Result<bool> {
        let Some(f) = &mut self.compute else {
            return Ok(false);
        };
        let value = f(rt)?;
        let changed = self.value.as_ref() != Some(&value);
        if changed {
            self.value = Some(value);
        }
        Ok(changed)
    }
}

type Choose<T, U> = Box<dyn Fn(&mut Builder<'_>, &T) -> Result<Cell<U>> + Send>;
pub(super) struct Switch<T: Value, U: Value> {
    pub outer: Id,
    pub choose: Choose<T, U>,
    pub last: Option<T>,
    pub selected: Option<Cell<U>>,
    pub selected_scope: Option<ScopeId>,
    pub value: Option<U>,
}
impl<T: Value, U: Value> Body for Switch<T, U> {
    fn value(&self) -> Option<&dyn Any> {
        self.value.as_ref().map(|v| -> &dyn Any { v })
    }
    fn scope(&self) -> Option<ScopeId> {
        self.selected_scope
    }
    fn prepare(&mut self, rt: &mut Runtime, id: Id) -> Result<()> {
        let outer = rt.value::<T>(self.outer)?.clone();
        if self.last.as_ref() == Some(&outer) {
            return Ok(());
        }
        rt.detach_selected(id)?;
        if let Some(scope) = self.selected_scope.take() {
            rt.invalidate_scope(scope);
        }
        self.selected = None;
        let scope = rt.new_scope()?;
        let previous = rt.current_scope.replace(scope);
        let selected = (self.choose)(&mut Builder { rt }, &outer);
        rt.current_scope = previous;
        let selected = selected?;
        rt.check(&selected)?;
        rt.attach_selected(id, &selected.lease)?;
        self.last = Some(outer);
        self.selected = Some(selected);
        self.selected_scope = Some(scope);
        Ok(())
    }
    fn evaluate(&mut self, rt: &Runtime) -> Result<bool> {
        let selected = self.selected.as_ref().ok_or(Error::Unsettled)?;
        let value = rt.value::<U>(selected.lease.id)?.clone();
        let changed = self.value.as_ref() != Some(&value);
        if changed {
            self.value = Some(value);
        }
        Ok(changed)
    }
}
